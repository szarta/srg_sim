//! Engine unit tests — split out of `engine.rs` (mechanical move; behavior
//! unchanged). Each `*_tests` module below is verbatim from `engine.rs`; the
//! module-level `use super::*;` re-exposes the engine module's scope (its own
//! items plus its private `use` imports) to those nested modules, so their own
//! `use super::*;` resolves exactly as it did in `engine.rs`.

use super::*;

#[cfg(test)]
mod breakout_modifier_tests {
    use super::*;

    fn deck(uuid: &str) -> Deck {
        serde_json::from_value(json!({
            "competitor": {
                "db_uuid": uuid, "name": uuid, "division": "World Championship",
                "stats": {"Power": 5, "Agility": 5, "Technique": 5,
                          "Submission": 5, "Grapple": 5, "Strike": 5},
            },
            "entrance": {"db_uuid": format!("{uuid}-ent"), "name": "ent"},
            "cards": [],
        }))
        .expect("deck")
    }

    /// A `Static` gimmick effect wrapping a single `BreakoutModifier`, gated by
    /// `condition` ("Always" by default).
    fn breakout_mod(delta: i64, attempts: Value, condition: Value) -> Value {
        json!({
            "@type": "Effect",
            "trigger": {"@type": "Static"},
            "condition": condition,
            "actions": [{"@type": "BreakoutModifier", "delta": delta, "attempts": attempts}],
            "duration": "WHILE_IN_PLAY",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "test", "source": "gimmick", "optional": false
        })
    }

    fn engine() -> Engine {
        let decider = Box::new(ReplayDecider::new(BTreeMap::new(), BTreeMap::new()));
        Engine::new(
            deck("A"),
            deck("B"),
            decider,
            1,
            String::new(),
            "sim".into(),
        )
    }

    fn push_gimmick(engine: &mut Engine, key: &str, eff: Value) {
        engine
            .state
            .players
            .get_mut(key)
            .unwrap()
            .competitor
            .effects
            .push(serde_json::from_value(eff).expect("effect"));
    }

    #[test]
    fn attempts_gate_selects_the_nth_roll() {
        // El Super Hombre V1: "Your 3rd breakout roll each turn is +2." Applies only
        // to the 3rd attempt; the 1st and 2nd see nothing.
        let mut engine = engine();
        push_gimmick(
            &mut engine,
            "A",
            breakout_mod(2, json!(3), json!({"@type": "Always"})),
        );
        assert_eq!(engine.breakout_bonus("A", 1, Skill::Strike), 0);
        assert_eq!(engine.breakout_bonus("A", 2, Skill::Strike), 0);
        assert_eq!(engine.breakout_bonus("A", 3, Skill::Strike), 2);
    }

    #[test]
    fn unattempted_modifier_applies_to_every_roll_and_stacks() {
        // A flat "your breakout rolls are +1" (attempts null) applies to all three,
        // and stacks additively with an attempt-gated modifier.
        let mut engine = engine();
        push_gimmick(
            &mut engine,
            "A",
            breakout_mod(1, Value::Null, json!({"@type": "Always"})),
        );
        push_gimmick(
            &mut engine,
            "A",
            breakout_mod(2, json!(3), json!({"@type": "Always"})),
        );
        assert_eq!(engine.breakout_bonus("A", 1, Skill::Strike), 1);
        assert_eq!(engine.breakout_bonus("A", 3, Skill::Strike), 3);
    }

    #[test]
    fn when_skill_gates_the_modifier_to_the_rolled_breakout_skill() {
        // The SRG Boss V3 / Pineapple: "Power is +1 during your breakout rolls" applies
        // only when the defender's breakout roll came up Power, not on the other five.
        let mut engine = engine();
        let modifier = json!({
            "@type": "Effect", "trigger": {"@type": "Static"},
            "condition": {"@type": "Always"},
            "actions": [{"@type": "BreakoutModifier", "delta": 1, "attempts": null,
                "when_skill": "Power"}],
            "duration": "WHILE_IN_PLAY",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "t", "source": "gimmick", "optional": false
        });
        push_gimmick(&mut engine, "A", modifier);
        assert_eq!(
            engine.breakout_bonus("A", 1, Skill::Power),
            1,
            "rolled Power → gated bonus applies"
        );
        assert_eq!(
            engine.breakout_bonus("A", 1, Skill::Agility),
            0,
            "rolled Agility → gated bonus does not apply"
        );
    }

    #[test]
    fn opp_directed_modifier_lands_on_the_defender() {
        // "Your opponent's breakout rolls are -2": a `who:OPP` modifier on B lowers A's
        // (the defender's) breakout roll, while a `who:SELF` mod on B does not touch A.
        let mut engine = engine();
        let opp_mod = json!({
            "@type": "Effect", "trigger": {"@type": "Static"},
            "condition": {"@type": "Always"},
            "actions": [{"@type": "BreakoutModifier", "delta": -2, "attempts": null,
                "when_skill": null, "who": "OPP"}],
            "duration": "WHILE_IN_PLAY",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "t", "source": "gimmick", "optional": false
        });
        push_gimmick(&mut engine, "B", opp_mod);
        assert_eq!(
            engine.breakout_bonus("A", 1, Skill::Strike),
            -2,
            "B's OPP-directed mod lowers A's breakout roll"
        );
        // A's own board carries no SelfSide mod, and B's mod is OPP-directed, so B's own
        // breakout roll is unaffected.
        assert_eq!(engine.breakout_bonus("B", 1, Skill::Strike), 0);
    }

    /// A `Grapple` `Lead` card for populating a per-count board.
    fn grapple(uuid: &str) -> Card {
        serde_json::from_value(json!({
            "atk_type": "Grapple", "db_uuid": uuid, "name": "G", "number": 1,
            "play_order": "Lead", "raw_text": "", "tags": [], "finish_bonuses": {},
            "effects": []
        }))
        .unwrap()
    }

    /// A per-count `BreakoutModifier` gimmick effect (schema v112).
    fn per_count_mod(delta: i64, attempts: Value, who: &str, per_who: &str, cap: Value) -> Value {
        json!({
            "@type": "Effect", "trigger": {"@type": "Static"},
            "condition": {"@type": "Always"},
            "actions": [{"@type": "BreakoutModifier", "delta": delta, "attempts": attempts,
                "when_skill": null, "who": who, "per_who": per_who, "cap": cap,
                "per": {"@type": "CardFilter", "number": null, "atk_type": "Grapple",
                        "play_order": null, "tag": null, "name": null, "raw": null}}],
            "duration": "WHILE_IN_PLAY",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "t", "source": "gimmick", "optional": false
        })
    }

    #[test]
    fn per_count_breakout_scales_by_the_counted_board_caps_and_gates_attempts() {
        let mut engine = engine();
        // Three Grapples on A's board (the per-count source), one on B's.
        for u in ["g1", "g2", "g3"] {
            engine
                .state
                .players
                .get_mut("A")
                .unwrap()
                .in_play
                .push(grapple(u));
        }
        engine
            .state
            .players
            .get_mut("B")
            .unwrap()
            .in_play
            .push(grapple("g4"));

        // B declares: "your opponent's breakout rolls are +1 for each Grapple they have in
        // play" — who=OPP (applies to B's opponent A), per_who=OPP (counts A's board).
        // 3 Grapples * 1 = 3, uncapped.
        push_gimmick(
            &mut engine,
            "B",
            per_count_mod(1, Value::Null, "OPP", "OPP", Value::Null),
        );
        assert_eq!(engine.breakout_bonus("A", 1, Skill::Strike), 3);

        // A caps its own per-count buff at +2 ("(Max +2)") over its own 3-Grapple board.
        push_gimmick(
            &mut engine,
            "A",
            per_count_mod(1, Value::Null, "SELF", "SELF", json!(2)),
        );
        // A now sees B's OPP mod (+3) plus its own capped SELF mod (min(3,2)=2) = +5.
        assert_eq!(engine.breakout_bonus("A", 1, Skill::Strike), 5);

        // An attempt-gated per-count mod on B (attempts=2) only bites A's 2nd roll: the 1st
        // sees +3 (B uncapped) +2 (A capped) = 5; the 2nd adds another +3 = 8.
        push_gimmick(
            &mut engine,
            "B",
            per_count_mod(1, json!(2), "OPP", "OPP", Value::Null),
        );
        assert_eq!(engine.breakout_bonus("A", 1, Skill::Strike), 5);
        assert_eq!(engine.breakout_bonus("A", 2, Skill::Strike), 8);
    }

    /// A `Static` gimmick `BreakoutAttempts` effect (schema v113): `set` overrides the
    /// base count, `delta` shifts it, `who` names the affected side.
    fn attempts_eff(delta: i64, set: Value, who: &str) -> Value {
        json!({
            "@type": "Effect", "trigger": {"@type": "Static"},
            "condition": {"@type": "Always"},
            "actions": [{"@type": "BreakoutAttempts", "delta": delta, "set": set, "who": who}],
            "duration": "WHILE_IN_PLAY",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "t", "source": "gimmick", "optional": false
        })
    }

    #[test]
    fn breakout_attempt_count_set_and_delta_from_both_boards() {
        // Baseline: no modifier → the default BREAKOUT_ATTEMPTS.
        let mut engine = engine();
        assert_eq!(engine.breakout_attempts_for("A"), BREAKOUT_ATTEMPTS);

        // B (the finisher) declares "your opponent gets 2 Breakout rolls" (set=2, who=OPP):
        // A, the defender, gets 2.
        push_gimmick(&mut engine, "B", attempts_eff(0, json!(2), "OPP"));
        assert_eq!(engine.breakout_attempts_for("A"), 2);
        // The same effect is OPP-directed, so it never touches B's own count.
        assert_eq!(engine.breakout_attempts_for("B"), BREAKOUT_ATTEMPTS);

        // A adds "you get 1 additional Breakout roll" (delta=+1, who=SELF): 2 + 1 = 3.
        push_gimmick(&mut engine, "A", attempts_eff(1, Value::Null, "SELF"));
        assert_eq!(engine.breakout_attempts_for("A"), 3);
    }

    #[test]
    fn breakout_attempt_count_floors_at_one() {
        // Two "1 fewer" from the finisher drive A's count from base 3 to 1; a third would
        // reach 0 but the count floors at 1 — a defender always gets at least one roll.
        let mut engine = engine();
        push_gimmick(&mut engine, "B", attempts_eff(-1, Value::Null, "OPP"));
        push_gimmick(&mut engine, "B", attempts_eff(-1, Value::Null, "OPP"));
        assert_eq!(engine.breakout_attempts_for("A"), 1);
        push_gimmick(&mut engine, "B", attempts_eff(-1, Value::Null, "OPP"));
        assert_eq!(engine.breakout_attempts_for("A"), 1);
    }

    /// A breakout `Reroll` (schema v102): the defender's own `who:SELF` and the
    /// finisher's `who:OPP` ("force your opponent to re-roll") both re-roll the
    /// DEFENDER's die, so both are offered when A is the defender; a `who:SELF` reroll
    /// sitting on the FINISHER never fires against A. A once-per-match guard is honored,
    /// and a board without any breakout reroll is a no-op.
    fn breakout_reroll(who: &str, freq: Value) -> Value {
        json!({
            "@type": "Effect", "trigger": {"@type": "Static"},
            "condition": {"@type": "Always"},
            "actions": [{"@type": "Reroll", "who": who, "once": false, "choose": false,
                "when": "THIS", "cost": null, "finish": false, "breakout": true}],
            "duration": "WHILE_IN_PLAY", "frequency": freq,
            "raw_clause": "re-roll your breakout roll", "source": "gimmick", "optional": false
        })
    }

    #[test]
    fn breakout_reroll_offered_for_defender_self_and_finisher_forced() {
        let unlimited = || json!({"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null});

        // No reroll anywhere → no-op for the defender A.
        let mut e = engine();
        assert!(!e.offer_breakout_reroll("A").expect("offer"));

        // Defender A's own "re-roll your Breakout roll".
        let mut e = engine();
        push_gimmick(&mut e, "A", breakout_reroll("SELF", unlimited()));
        assert!(e.offer_breakout_reroll("A").expect("offer"));

        // Finisher B's "force your opponent (A) to re-roll their Breakout roll".
        let mut e = engine();
        push_gimmick(&mut e, "B", breakout_reroll("OPP", unlimited()));
        assert!(e.offer_breakout_reroll("A").expect("offer"));

        // A `who:SELF` breakout reroll on the FINISHER B is for B's own die, not A's —
        // never offered when A is the defender.
        let mut e = engine();
        push_gimmick(&mut e, "B", breakout_reroll("SELF", unlimited()));
        assert!(!e.offer_breakout_reroll("A").expect("offer"));

        // The once-per-match guard: fires once, then not again.
        let once = json!({"@type": "FrequencyGuard", "kind": "ONCE_PER_MATCH", "n": null});
        let mut e = engine();
        push_gimmick(&mut e, "A", breakout_reroll("SELF", once));
        assert!(e.offer_breakout_reroll("A").expect("first"));
        assert!(!e.offer_breakout_reroll("A").expect("second"));
    }

    /// Re-roll hand-cost affordability (schema v103): a `BuryFromHand`/`DiscardFromHand`
    /// cost is payable only while the hand holds `count` (matching) cards; a
    /// `ShuffleInPlay` cost needs a matching in-play card. Payment itself delegates to
    /// the already-tested `bury_from_hand`/`discard_from_hand`/`pay_reroll_cost`.
    #[test]
    fn reroll_hand_cost_affordability() {
        let cost = |kind, count, filter| RerollCost {
            node_type: crate::ir::RerollCostTag,
            kind,
            count,
            filter,
        };
        let mut e = engine();
        for c in strike_cards(3) {
            e.state.players.get_mut("A").unwrap().hand.push(c);
        }
        // 3 Strikes in hand: bury/discard up to 3, not 4.
        assert!(e.can_pay_reroll("A", &cost(RerollCostKind::BuryFromHand, Some(3), None)));
        assert!(!e.can_pay_reroll("A", &cost(RerollCostKind::BuryFromHand, Some(4), None)));
        assert!(e.can_pay_reroll("A", &cost(RerollCostKind::DiscardFromHand, Some(2), None)));
        // Typed cost: no Submission in hand -> unaffordable.
        let subm = CardFilter {
            atk_type: Some(AtkType::Submission),
            ..Default::default()
        };
        assert!(!e.can_pay_reroll(
            "A",
            &cost(RerollCostKind::DiscardFromHand, Some(1), Some(subm))
        ));
        // ShuffleInPlay with no matching in-play card -> unaffordable.
        let potion = CardFilter {
            name_contains: vec!["Potion".to_owned()],
            ..Default::default()
        };
        assert!(!e.can_pay_reroll(
            "A",
            &cost(RerollCostKind::ShuffleInPlay, None, Some(potion))
        ));
    }

    /// OnReroll dispatch (schema v104): `run_on_reroll(target)` fires the target's own
    /// `OnReroll{SelfSide}` and the opponent's `OnReroll{Opp}`, summing roll-modifier
    /// deltas (applied to the re-rolled value) while other actions resolve. A board with
    /// no OnReroll returns delta 0.
    #[test]
    fn on_reroll_dispatch_sums_deltas_from_both_sides() {
        fn reroll_mod(who: &str, mod_who: &str, delta: i64) -> Value {
            json!({
                "@type": "Effect", "trigger": {"@type": "OnReroll", "who": who},
                "condition": {"@type": "Always"},
                "actions": [{"@type": "ModifyRoll", "who": mod_who, "delta": delta,
                    "when": "THIS", "per": null, "per_who": "OPP", "per_zone": "IN_PLAY"}],
                "duration": "WHILE_IN_PLAY",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "t", "source": "gimmick", "optional": false
            })
        }

        // No OnReroll anywhere → delta 0.
        let mut e = engine();
        assert_eq!(e.run_on_reroll("A").expect("none"), 0);

        // A's own "when you re-roll, your roll is +2" fires on A's reroll.
        let mut e = engine();
        push_gimmick(&mut e, "A", reroll_mod("SELF", "SELF", 2));
        assert_eq!(e.run_on_reroll("A").expect("self"), 2);
        // ...but not on B's reroll (A's SelfSide keys off A's own die).
        assert_eq!(e.run_on_reroll("B").expect("self-not-b"), 0);

        // B's "when your opponent re-rolls, their roll is -1" fires on A's reroll.
        let mut e = engine();
        push_gimmick(&mut e, "B", reroll_mod("OPP", "OPP", -1));
        assert_eq!(e.run_on_reroll("A").expect("opp"), -1);

        // Both sides stack on A's reroll: +2 (A self) and -1 (B opp) → +1.
        let mut e = engine();
        push_gimmick(&mut e, "A", reroll_mod("SELF", "SELF", 2));
        push_gimmick(&mut e, "B", reroll_mod("OPP", "OPP", -1));
        assert_eq!(e.run_on_reroll("A").expect("both"), 1);
    }

    fn strike_cards(n: usize) -> Vec<Card> {
        (0..n)
            .map(|i| {
                serde_json::from_value(json!({
                    "atk_type": "Strike", "db_uuid": format!("s{i}"), "effects": [],
                    "finish_bonuses": {}, "name": "s", "number": 1, "play_order": "Lead",
                    "raw_text": "", "tags": []
                }))
                .expect("card")
            })
            .collect()
    }

    fn finish_roll_per(zone: &str, divisor: Value) -> Value {
        json!({
            "@type": "Effect",
            "trigger": {"@type": "Static"},
            "condition": {"@type": "Always"},
            "actions": [{"@type": "FinishRollBonus", "delta": 1, "when_skill": null,
                "either": false, "when_base_le": null, "when_base_ge": null,
                "per": {"@type": "CardFilter", "atk_type": "Strike"},
                "per_who": "SELF", "per_zone": zone, "per_divisor": divisor}],
            "duration": "WHILE_IN_PLAY",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "t", "source": "gimmick", "optional": false
        })
    }

    #[test]
    fn finish_roll_bonus_counts_cards_flipped_this_turn() {
        // Five Star Frog Splash: "+1 for each Strike card flipped" reads the turn's
        // flips (CountZone::FlippedThisTurn), not the discard they land in.
        let mut engine = engine();
        push_gimmick(
            &mut engine,
            "A",
            finish_roll_per("FLIPPED_THIS_TURN", Value::Null),
        );
        assert_eq!(
            engine.finish_roll_bonus("A", Skill::Grapple, 5),
            0,
            "no flips yet"
        );
        engine.state.players.get_mut("A").unwrap().flipped_this_turn = strike_cards(3);
        assert_eq!(
            engine.finish_roll_bonus("A", Skill::Grapple, 5),
            3,
            "+1 per Strike flipped"
        );
    }

    #[test]
    fn finish_roll_bonus_divisor_floors_the_count() {
        // The Ride Along: "+1 for every 3 Strikes you have in play" -> floor(7/3) = 2.
        let mut engine = engine();
        push_gimmick(&mut engine, "A", finish_roll_per("IN_PLAY", json!(3)));
        engine.state.players.get_mut("A").unwrap().in_play = strike_cards(7);
        assert_eq!(
            engine.finish_roll_bonus("A", Skill::Grapple, 5),
            2,
            "floor(7/3)"
        );
    }

    #[test]
    fn finish_roll_bonus_per_crowd_adds_the_live_crowd_meter_capped() {
        // "Your Finish roll is + the Crowd Meter (Max +2)" (task #131): a SECOND
        // crowd-meter addend (the finish math already folds in the first), read live off
        // the Crowd Meter and clamped to the cap.
        let mut engine = engine();
        push_gimmick(
            &mut engine,
            "A",
            json!({
                "@type": "Effect", "trigger": {"@type": "Static"},
                "condition": {"@type": "Always"},
                "actions": [{"@type": "FinishRollBonus", "delta": 0, "when_skill": null,
                    "either": false, "when_base_le": null, "when_base_ge": null,
                    "per": null, "per_who": "SELF", "per_zone": "IN_PLAY", "per_divisor": null,
                    "cap": 2, "per_crowd": true}],
                "duration": "WHILE_IN_PLAY",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "t", "source": "gimmick", "optional": false
            }),
        );
        engine.state.crowd_meter = 0;
        assert_eq!(
            engine.finish_roll_bonus("A", Skill::Grapple, 5),
            0,
            "no crowd, no extra bonus"
        );
        engine.state.crowd_meter = 1;
        assert_eq!(
            engine.finish_roll_bonus("A", Skill::Grapple, 5),
            1,
            "tracks the live Crowd Meter"
        );
        engine.state.crowd_meter = 5;
        assert_eq!(
            engine.finish_roll_bonus("A", Skill::Grapple, 5),
            2,
            "clamped to Max +2"
        );
    }

    #[test]
    fn false_condition_and_wrong_side_do_not_count() {
        // A gated modifier whose condition is false contributes nothing, and a
        // modifier on B never leaks into A's breakout (each reads its own standing set).
        let mut engine = engine();
        push_gimmick(
            &mut engine,
            "A",
            breakout_mod(
                2,
                Value::Null,
                json!({"@type": "CrowdMeterCompare", "cmp": ">=", "value": 5}),
            ),
        );
        push_gimmick(
            &mut engine,
            "B",
            breakout_mod(4, Value::Null, json!({"@type": "Always"})),
        );
        assert_eq!(engine.breakout_bonus("A", 1, Skill::Strike), 0);
        assert_eq!(engine.breakout_bonus("B", 1, Skill::Strike), 4);
    }

    #[test]
    fn blanked_gimmick_suppresses_the_modifier() {
        // A blanked gimmick contributes no breakout modifier (standing_effects skips it).
        let mut engine = engine();
        push_gimmick(
            &mut engine,
            "A",
            breakout_mod(2, json!(3), json!({"@type": "Always"})),
        );
        engine.state.players.get_mut("A").unwrap().gimmick_blanked = true;
        assert_eq!(engine.breakout_bonus("A", 3, Skill::Strike), 0);
    }

    #[test]
    fn breakout_roll_honors_the_modifier() {
        // The defender's stats are all 5, so a finish of 8 is unbreakable (5 < 8) with
        // no modifier — but a flat +5 breakout modifier lifts every roll to 10 and
        // breaks out on the first attempt. Drives the real `breakout()` roll, proving
        // the bonus reaches `stat_breaks_out` as a negative penalty.
        let mut engine = engine();
        assert!(
            !engine.breakout("A", 8).unwrap(),
            "5 < 8 cannot break out unaided"
        );
        push_gimmick(
            &mut engine,
            "A",
            breakout_mod(5, Value::Null, json!({"@type": "Always"})),
        );
        assert!(
            engine.breakout("A", 8).unwrap(),
            "+5 lifts the roll to 10 and breaks out"
        );
        // The applied modifier is recorded as a negative penalty on the roll.
        let Some(Event::Breakout { rolls, .. }) = engine.log.events.last() else {
            panic!("last event is a Breakout");
        };
        assert_eq!(rolls[0].penalty, -5);
    }

    #[test]
    fn opponent_rolling_ten_on_breakout_fires_the_loss() {
        // "If your opponent rolls 10 for their Breakout roll, you lose via
        // disqualification" (task #94): A (finisher) holds the clause; B (defender)
        // rolls a raw 10 and A loses the match.
        let mut engine = engine();
        // Every one of B's stats is 10, so any breakout skill die is a raw-10 roll.
        let stats = &mut engine.state.players.get_mut("B").unwrap().competitor.stats;
        *stats = Skills {
            power: 10,
            agility: 10,
            technique: 10,
            submission: 10,
            grapple: 10,
            strike: 10,
        };
        // A's in-play finish carries the OnBreakoutRoll(Opp) + RollValue(10) loss.
        let card = json!({
            "atk_type": "Strike", "db_uuid": "brk", "name": "brk", "number": 1,
            "play_order": "Finish", "raw_text": "", "tags": [], "finish_bonuses": {},
            "effects": [{
                "@type": "Effect",
                "trigger": {"@type": "OnBreakoutRoll", "who": "OPP"},
                "condition": {"@type": "RollValue", "cmp": "=", "value": 10},
                "actions": [{"@type": "LoseBy", "kind": "DISQUALIFICATION", "who": "SELF"}],
                "duration": "INSTANT",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "test", "source": "card", "optional": false
            }]
        });
        engine
            .state
            .players
            .get_mut("A")
            .unwrap()
            .in_play
            .push(serde_json::from_value(card).unwrap());
        engine.breakout("B", 8).unwrap();
        assert!(engine.ended(), "the rolled 10 ended the match on the roll");
        assert_eq!(
            engine.result.as_ref().unwrap().winner,
            "B",
            "A took the disqualification loss, so B wins"
        );
    }

    #[test]
    fn reactive_breakout_reroll_rerolls_the_current_attempt() {
        // My Most Powerful Spell: "When your Nth Breakout roll is 5 or 6, you may re-roll
        // your Breakout roll." A POST-roll reactive re-roll — distinct from the standing
        // pre-roll `offer_breakout_reroll`. Setup makes it deterministic: A's stats are
        // all 5, so EVERY breakout roll has value 5 (fails vs finish 8) and the
        // `RollValue{5}` gate always holds post-roll. The gate is `None`-false so the
        // pre-roll offer skips it; only `run_on_breakout_roll` fires it. A paired
        // CrowdMeter+1 counts fires: 3 attempts × (1 initial + 3 re-rolls) = 12.
        let mut engine = engine();
        let stats = &mut engine.state.players.get_mut("A").unwrap().competitor.stats;
        *stats = Skills {
            power: 5,
            agility: 5,
            technique: 5,
            submission: 5,
            grapple: 5,
            strike: 5,
        };
        push_gimmick(
            &mut engine,
            "A",
            json!({
                "@type": "Effect",
                "trigger": {"@type": "OnBreakoutRoll", "who": "SELF"},
                "condition": {"@type": "RollValue", "cmp": "=", "value": 5},
                "actions": [
                    {"@type": "CrowdMeter", "delta": 1},
                    {"@type": "Reroll", "who": "SELF", "once": false, "breakout": true}
                ],
                "duration": "INSTANT",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "reactive breakout reroll", "source": "gimmick", "optional": false
            }),
        );
        assert!(
            !engine.breakout("A", 8).unwrap(),
            "value 5 < 8 never breaks out even with re-rolls"
        );
        assert_eq!(
            engine.state.crowd_meter, 12,
            "3 attempts × (1 initial + 3 re-rolls) = 12 fires of the reactive re-roll"
        );
    }

    #[test]
    fn bury_this_card_moves_the_stopped_card_to_deck_bottom() {
        // "bury this card" (task #94): the stopped card leaves the discard for the
        // bottom of its owner's deck.
        let mut engine = engine();
        let card: Card = serde_json::from_value(json!({
            "atk_type": "Strike", "db_uuid": "stp", "name": "stp", "number": 1,
            "play_order": "Finish", "raw_text": "", "tags": [], "finish_bonuses": {},
            "effects": []
        }))
        .unwrap();
        engine
            .state
            .players
            .get_mut("A")
            .unwrap()
            .discard
            .push(card);
        engine.stopped_card = Some("stp".to_owned());
        engine.act_bury_this_card("A");
        let a = &engine.state.players["A"];
        assert!(
            a.discard.iter().all(|c| c.db_uuid != "stp"),
            "the stopped card left the discard"
        );
        assert_eq!(
            a.deck.last().unwrap().db_uuid,
            "stp",
            "buried to the bottom of the deck"
        );
    }

    /// "The next time you roll <S>, it is +N": the mod waits across rolls of other
    /// skills, applies once to the first roll that comes up its skill, and is consumed.
    #[test]
    fn skill_keyed_roll_mod_waits_for_its_skill_then_is_consumed() {
        let mut engine = engine();
        // Queue "+3 the next time A rolls Technique" (as act_modify_roll would).
        engine
            .state
            .players
            .get_mut("A")
            .unwrap()
            .pending_skill_roll_mods
            .push(SkillRollMod {
                skill: Skill::Technique,
                delta: 3,
            });

        let mut fired = false;
        for _ in 0..500 {
            let queued_before = engine.state.players["A"].pending_skill_roll_mods.len();
            let (skill, value) = engine.roll_for("A", true);
            let base = engine.stat("A", skill) + engine.turn_roll_bonus("A", skill);
            if skill == Skill::Technique && !fired {
                assert_eq!(value, base + 3, "the Technique roll gets +3");
                assert!(
                    engine.state.players["A"].pending_skill_roll_mods.is_empty(),
                    "the mod is consumed once its skill is rolled"
                );
                fired = true;
            } else if !fired {
                assert_eq!(value, base, "a non-Technique roll is unmodified");
                assert_eq!(
                    engine.state.players["A"].pending_skill_roll_mods.len(),
                    queued_before,
                    "the mod stays queued until its skill comes up"
                );
            }
        }
        assert!(fired, "Technique came up within 500 rolls");
    }

    /// `consume_skill_roll_mod` drains every entry for the rolled skill (summing their
    /// deltas) and leaves entries for other skills untouched.
    #[test]
    fn consume_skill_roll_mod_drains_matching_and_sums() {
        let mut engine = engine();
        let mods = &mut engine
            .state
            .players
            .get_mut("A")
            .unwrap()
            .pending_skill_roll_mods;
        mods.push(SkillRollMod {
            skill: Skill::Power,
            delta: 2,
        });
        mods.push(SkillRollMod {
            skill: Skill::Power,
            delta: 5,
        });
        mods.push(SkillRollMod {
            skill: Skill::Grapple,
            delta: 1,
        });

        // Rolling Power drains both Power entries (2 + 5) and keeps Grapple.
        assert_eq!(engine.consume_skill_roll_mod("A", Skill::Power), 7);
        let left = &engine.state.players["A"].pending_skill_roll_mods;
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].skill, Skill::Grapple);
        // A skill with no queued mod returns 0 and changes nothing.
        assert_eq!(engine.consume_skill_roll_mod("A", Skill::Strike), 0);
        assert_eq!(engine.state.players["A"].pending_skill_roll_mods.len(), 1);
    }

    /// A pending roll-conditional draw ("if your [opponent's] next turn roll is <S>,
    /// draw N") fires when the WATCHED side's resolved turn roll comes up its skill, is
    /// consumed even on a non-match (a one-turn window), and watches the opponent's roll
    /// when armed with `watch = Opp`.
    #[test]
    fn pending_roll_draw_fires_on_a_match_and_fizzles_otherwise() {
        let make_engine = engine; // bind the ctor before a local `engine` shadows it
        let card = |u: &str| -> Card {
            serde_json::from_value(json!({
                "db_uuid": u, "name": u, "number": 1, "atk_type": "Strike",
                "play_order": "Lead", "finish_bonuses": {}, "effects": []
            }))
            .expect("card")
        };
        let ctx = |s: Skill| RollContext {
            skill: Some(s),
            gap: None,
            value: Some(10),
            opp_skill: None,
        };
        let arm = |engine: &mut Engine, skill: Skill, count: i64, watch: Who| {
            engine
                .state
                .players
                .get_mut("A")
                .unwrap()
                .pending_roll_draws
                .push(PendingRollDraw {
                    skill,
                    count,
                    watch,
                });
        };
        let stock = |engine: &mut Engine, n: usize| {
            for i in 0..n {
                engine
                    .state
                    .players
                    .get_mut("A")
                    .unwrap()
                    .deck
                    .push(card(&format!("d{i}")));
            }
        };

        // Match: A armed "if your next turn roll is Grapple, draw 1"; A rolls Grapple.
        let mut engine = make_engine();
        stock(&mut engine, 3);
        arm(&mut engine, Skill::Grapple, 1, Who::SelfSide);
        engine.roll_ctx.insert("A".into(), ctx(Skill::Grapple));
        engine.roll_ctx.insert("B".into(), ctx(Skill::Power));
        let before = engine.state.players["A"].hand.len();
        engine.resolve_pending_roll_draws().unwrap();
        assert_eq!(
            engine.state.players["A"].hand.len(),
            before + 1,
            "drew on the matching roll"
        );
        assert!(
            engine.state.players["A"].pending_roll_draws.is_empty(),
            "consumed after firing"
        );

        // Fizzle: armed Grapple but A rolls Power -> no draw, still consumed.
        let mut engine = make_engine();
        stock(&mut engine, 3);
        arm(&mut engine, Skill::Grapple, 1, Who::SelfSide);
        engine.roll_ctx.insert("A".into(), ctx(Skill::Power));
        let before = engine.state.players["A"].hand.len();
        engine.resolve_pending_roll_draws().unwrap();
        assert_eq!(
            engine.state.players["A"].hand.len(),
            before,
            "no draw on a non-matching roll"
        );
        assert!(
            engine.state.players["A"].pending_roll_draws.is_empty(),
            "consumed even on a fizzle (the next-turn-roll window closed)"
        );

        // Opponent-watch: A armed watch=Opp; A draws off the OPPONENT's roll skill.
        let mut engine = make_engine();
        stock(&mut engine, 3);
        arm(&mut engine, Skill::Strike, 2, Who::Opp);
        engine.roll_ctx.insert("A".into(), ctx(Skill::Power));
        engine.roll_ctx.insert("B".into(), ctx(Skill::Strike));
        let before = engine.state.players["A"].hand.len();
        engine.resolve_pending_roll_draws().unwrap();
        assert_eq!(
            engine.state.players["A"].hand.len(),
            before + 2,
            "A drew 2 off the opponent's Strike roll"
        );
    }

    /// A one-turn skill-gated turn-roll bonus sums matching entries, is gated by its
    /// skill set, is drained by `consume_pending` (the one-turn window), and lands on the
    /// AFFECTED player when armed with `who = Opp`.
    #[test]
    fn next_roll_skill_bonus_sums_gates_and_is_drained() {
        let mut engine = engine();
        let q = &mut engine
            .state
            .players
            .get_mut("A")
            .unwrap()
            .pending_next_roll_skill_mods;
        q.push(SkillSetRollMod {
            skills: vec![Skill::Grapple, Skill::Power],
            delta: 2,
        });
        q.push(SkillSetRollMod {
            skills: vec![Skill::Grapple],
            delta: 1,
        });
        q.push(SkillSetRollMod {
            skills: vec![Skill::Agility],
            delta: 3,
        });

        // Grapple matches both Grapple entries (2 + 1); Power one (2); Agility one (3);
        // an unlisted skill gets nothing.
        assert_eq!(engine.next_roll_skill_bonus("A", Skill::Grapple), 3);
        assert_eq!(engine.next_roll_skill_bonus("A", Skill::Power), 2);
        assert_eq!(engine.next_roll_skill_bonus("A", Skill::Agility), 3);
        assert_eq!(engine.next_roll_skill_bonus("A", Skill::Strike), 0, "gated");

        // `consume_pending` drains the whole queue — the next-turn-roll window closed.
        engine.consume_pending();
        assert!(engine.state.players["A"]
            .pending_next_roll_skill_mods
            .is_empty());
        assert_eq!(engine.next_roll_skill_bonus("A", Skill::Grapple), 0);

        // `who = Opp` stores the mod on the opponent, whose roll it modifies.
        engine.act_next_roll_skill_bonus(Who::Opp, &[Skill::Strike], -2, "A");
        assert!(engine.state.players["A"]
            .pending_next_roll_skill_mods
            .is_empty());
        assert_eq!(engine.next_roll_skill_bonus("B", Skill::Strike), -2);
    }

    /// A multi-turn bonus applies its `delta` on each of the next N roll-offs (skill-
    /// agnostic), decrements once per `consume_pending`, expires after N, and lands on the
    /// opponent when armed with `who = Opp`.
    #[test]
    fn multi_turn_roll_bonus_applies_for_n_rolls_then_expires() {
        let mut engine = engine();
        // "Your opponent's next 3 turn rolls are -1" armed by A -> stored on B.
        engine.act_multi_turn_roll_bonus(Who::Opp, 3, -1, "A");
        assert!(
            engine.state.players["A"].multi_turn_roll_mods.is_empty(),
            "armed against the opponent, not the source"
        );
        assert_eq!(
            engine.state.players["B"].multi_turn_roll_mods[0].remaining,
            3
        );

        // Applies (skill-agnostic) on each of B's next three roll-offs, then expires.
        for expect_remaining in [3, 2, 1] {
            assert_eq!(
                engine.multi_turn_roll_bonus("B"),
                -1,
                "active while remaining = {expect_remaining}"
            );
            engine.consume_pending(); // one roll-off spent
        }
        assert_eq!(
            engine.multi_turn_roll_bonus("B"),
            0,
            "expired after 3 rolls"
        );
        assert!(engine.state.players["B"].multi_turn_roll_mods.is_empty());

        // A zero-length arm adds nothing.
        engine.act_multi_turn_roll_bonus(Who::SelfSide, 0, 5, "A");
        assert!(engine.state.players["A"].multi_turn_roll_mods.is_empty());
    }

    /// `act_mill_deck` moves cards from the named DECK END to discard, with no flip
    /// side effects (nothing recorded in `flipped_this_turn`).
    #[test]
    fn act_mill_deck_takes_from_the_named_end_without_flip_semantics() {
        let mut engine = engine();
        let card = |u: &str| -> Card {
            serde_json::from_value(json!({
                "db_uuid": u, "name": u, "number": 1, "atk_type": "Strike",
                "play_order": "Lead", "finish_bonuses": {}, "effects": []
            }))
            .unwrap()
        };
        {
            let deck = &mut engine.state.players.get_mut("A").unwrap().deck;
            *deck = vec![card("top"), card("mid"), card("bot")];
        }
        engine.act_mill_deck(Who::SelfSide, 1, DeckEnd::Bottom, "A");
        let a = &engine.state.players["A"];
        // The BOTTOM card left the deck for the discard; top/mid remain, in order.
        assert_eq!(
            a.deck
                .iter()
                .map(|c| c.db_uuid.as_str())
                .collect::<Vec<_>>(),
            ["top", "mid"]
        );
        assert_eq!(a.discard.last().unwrap().db_uuid, "bot");
        // A mill is NOT a flip — nothing recorded for FlippedThisTurn counters.
        assert!(
            a.flipped_this_turn.is_empty(),
            "mill must not count as a flip"
        );
    }

    /// `act_reveal` marks the chosen hand card(s) in `revealed_hand`. With a single-card
    /// hand the `reveal` decision auto-resolves, so no decider is consulted.
    #[test]
    fn act_reveal_marks_the_chosen_card_revealed() {
        let mut engine = engine();
        let card: Card = serde_json::from_value(json!({
            "db_uuid": "rv", "name": "rv", "number": 1, "atk_type": "Strike",
            "play_order": "Lead", "finish_bonuses": {}, "effects": []
        }))
        .unwrap();
        engine.state.players.get_mut("A").unwrap().hand.push(card);

        engine
            .act_reveal(Who::SelfSide, 1, false, "A")
            .expect("reveal");
        assert!(engine.state.players["A"].revealed_hand.contains("rv"));

        // The reveal exposes it to B's observable projection.
        let a = engine.state.observable("B");
        let revealed = a["players"]["A"]["revealed"].as_array().expect("revealed");
        assert_eq!(revealed[0]["db_uuid"], "rv");
    }
}

#[cfg(test)]
mod on_stop_order_tests {
    use super::*;

    fn card(uuid: &str, order: &str) -> Value {
        json!({
            "atk_type": "Strike", "db_uuid": uuid, "effects": [], "finish_bonuses": {},
            "name": uuid, "number": 1, "play_order": order, "raw_text": "", "tags": []
        })
    }

    /// La Fenix (Super Lucha): A's gimmick tutors a Finish to hand when A's *Finish*
    /// is stopped (`OnStop{dir: YOURS, order: Finish}`). A's deck holds one Finish
    /// (the tutor target) and one Lead.
    fn la_fenix_engine() -> Engine {
        let gimmick = json!({
            "@type": "Effect",
            "trigger": {"@type": "OnStop", "dir": "YOURS", "order": "Finish"},
            "condition": {"@type": "Always"},
            "actions": [{"@type": "Search",
                "filter": {"@type": "CardFilter", "number": null, "atk_type": null,
                           "play_order": "Finish", "tag": null, "name": null, "raw": null},
                "dest": "HAND", "count": 1}],
            "duration": "INSTANT",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "test", "source": "gimmick", "optional": false
        });
        let deck_a: Deck = serde_json::from_value(json!({
            "competitor": {"db_uuid": "LF", "name": "La Fenix", "division": "World Championship",
                "stats": {"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5},
                "effects": [gimmick]},
            "entrance": {"db_uuid": "LF-ent", "name": "ent"},
            "cards": [card("tutor-finish", "Finish"), card("some-lead", "Lead")],
        }))
        .expect("deck A");
        let deck_b: Deck = serde_json::from_value(json!({
            "competitor": {"db_uuid": "B", "name": "B", "division": "World Championship",
                "stats": {"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5}},
            "entrance": {"db_uuid": "B-ent", "name": "ent"}, "cards": [],
        }))
        .expect("deck B");
        let decider = Box::new(ReplayDecider::new(BTreeMap::new(), BTreeMap::new()));
        Engine::new(deck_a, deck_b, decider, 1, String::new(), "sim".into())
    }

    fn tutored(engine: &Engine) -> bool {
        engine.state.players["A"]
            .hand
            .iter()
            .any(|c| c.db_uuid == "tutor-finish")
    }

    #[test]
    fn stopping_a_finish_fires_the_order_gated_tutor() {
        let mut engine = la_fenix_engine();
        let attack: Card = serde_json::from_value(card("my-finish", "Finish")).unwrap();
        let stop: Card = serde_json::from_value(card("their-stop", "Lead")).unwrap();
        engine.apply_stop("A", "B", attack, stop, vec![]).unwrap();
        assert!(
            tutored(&engine),
            "a stopped Finish tutors the deck Finish to hand"
        );
    }

    #[test]
    fn stopping_a_lead_does_not_fire_the_finish_gated_tutor() {
        let mut engine = la_fenix_engine();
        let attack: Card = serde_json::from_value(card("my-lead", "Lead")).unwrap();
        let stop: Card = serde_json::from_value(card("their-stop", "Lead")).unwrap();
        engine.apply_stop("A", "B", attack, stop, vec![]).unwrap();
        assert!(
            !tutored(&engine),
            "the order=Finish gate stays inert when a Lead is stopped"
        );
    }

    /// "Search your deck OR discard pile": a `Search{source: DeckOrDiscard}` finds a
    /// matching card sitting in the discard pile and moves it to hand.
    #[test]
    fn deck_or_discard_search_pulls_from_the_discard_pile() {
        let mut engine = la_fenix_engine();
        {
            let a = engine.state.players.get_mut("A").unwrap();
            a.deck.clear(); // isolate: the only candidate lives in the discard pile
            a.discard
                .push(serde_json::from_value(card("recalled", "Lead")).unwrap());
        }
        engine
            .act_search(
                &CardFilter::default(),
                Dest::Hand,
                1,
                SearchSource::DeckOrDiscard,
                "A",
            )
            .expect("search");
        assert!(
            engine.state.players["A"]
                .hand
                .iter()
                .any(|c| c.db_uuid == "recalled"),
            "the discard-pile card was tutored to hand"
        );
        assert!(
            !engine.state.players["A"]
                .discard
                .iter()
                .any(|c| c.db_uuid == "recalled"),
            "it left the discard pile"
        );
    }
}

#[cfg(test)]
mod on_shuffle_tests {
    use super::*;

    fn card(uuid: &str) -> Value {
        json!({
            "atk_type": "Strike", "db_uuid": uuid, "effects": [], "finish_bonuses": {},
            "name": uuid, "number": 1, "play_order": "Lead", "raw_text": "", "tags": []
        })
    }

    /// Memes Dealer V2 on A: `OnShuffle{who=OPP}` → Draw 2, so A draws whenever B's
    /// deck is shuffled by an effect. Both decks hold cards so the draw is observable.
    fn memes_engine() -> Engine {
        let gimmick = json!({
            "@type": "Effect",
            "trigger": {"@type": "OnShuffle", "who": "OPP"},
            "condition": {"@type": "Always"},
            "actions": [{"@type": "Draw", "n": 2, "source": "TOP", "who": "SELF",
                         "per": null, "per_who": "SELF"}],
            "duration": "INSTANT",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "test", "source": "gimmick", "optional": false
        });
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let cards: Vec<Value> = (0..10).map(|i| card(&format!("c{i}"))).collect();
        let deck_a: Deck = serde_json::from_value(json!({
            "competitor": {"db_uuid": "MD", "name": "Memes", "division": "Underworld",
                "stats": stats, "effects": [gimmick]},
            "entrance": {"db_uuid": "MD-ent", "name": "ent"}, "cards": cards.clone(),
        }))
        .expect("deck A");
        let deck_b: Deck = serde_json::from_value(json!({
            "competitor": {"db_uuid": "B", "name": "B", "division": "Underworld", "stats": stats},
            "entrance": {"db_uuid": "B-ent", "name": "ent"}, "cards": cards,
        }))
        .expect("deck B");
        let decider = Box::new(ReplayDecider::new(BTreeMap::new(), BTreeMap::new()));
        Engine::new(deck_a, deck_b, decider, 1, String::new(), "sim".into())
    }

    fn hand(engine: &Engine, key: &str) -> usize {
        engine.state.players[key].hand.len()
    }

    #[test]
    fn opponents_effect_shuffle_fires_the_draw() {
        // B shuffles their own deck via an effect -> A (the opponent) draws 2.
        let mut engine = memes_engine();
        engine.act_shuffle_deck(Who::SelfSide, "B").unwrap();
        assert_eq!(hand(&engine, "A"), 2, "A draws 2 when B's deck is shuffled");
    }

    #[test]
    fn own_shuffle_does_not_fire_the_opp_gated_draw() {
        // A shuffling their OWN deck must not fire A's who=OPP OnShuffle.
        let mut engine = memes_engine();
        engine.act_shuffle_deck(Who::SelfSide, "A").unwrap();
        assert_eq!(
            hand(&engine, "A"),
            0,
            "who=OPP does not fire on your own shuffle"
        );
    }

    #[test]
    fn setup_shuffle_does_not_fire_on_shuffle() {
        // The match-start setup shuffle bypasses OnShuffle: A gets only its opening hand.
        let mut engine = memes_engine();
        engine.setup().unwrap();
        assert_eq!(
            hand(&engine, "A"),
            OPENING_HAND,
            "setup shuffle draws no OnShuffle bonus"
        );
    }
}

#[cfg(test)]
mod on_discard_move_tests {
    use super::*;

    /// Always takes the first legal option — these tests exercise the trigger's
    /// firing, not the choice, and every decision point here is a card pick.
    struct FirstLegal;

    impl Decider for FirstLegal {
        fn decide(
            &mut self,
            _point: &str,
            _viewer: &str,
            legal: &[Value],
            _state: &mut GameState,
        ) -> Option<Value> {
            legal.first().cloned()
        }

        fn policy_name(&self, _viewer: &str) -> String {
            "first-legal".to_owned()
        }
    }

    fn card(uuid: &str) -> Value {
        json!({
            "atk_type": "Strike", "db_uuid": uuid, "effects": [], "finish_bonuses": {},
            "name": uuid, "number": 1, "play_order": "Lead", "raw_text": "", "tags": []
        })
    }

    /// Brumeister V2 on A: `OnDiscardMove{who=OPP}` → `RemoveFromPlay{OPP, 1}`, so A
    /// discards one of B's in-play cards whenever an effect pulls cards out of B's
    /// discard pile. B starts with a stocked discard pile and two cards in play.
    fn brumeister_engine() -> Engine {
        let gimmick = json!({
            "@type": "Effect",
            "trigger": {"@type": "OnDiscardMove", "who": "OPP"},
            "condition": {"@type": "Always"},
            "actions": [{"@type": "RemoveFromPlay", "who": "OPP", "count": 1,
                         "selector": {"@type": "CardFilter", "number": null, "atk_type": null,
                                      "play_order": null, "tag": null, "name": null, "raw": null,
                                      "name_contains": [], "text_contains": []}}],
            "duration": "INSTANT",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "test", "source": "gimmick", "optional": false
        });
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let cards: Vec<Value> = (0..10).map(|i| card(&format!("c{i}"))).collect();
        let deck_a: Deck = serde_json::from_value(json!({
            "competitor": {"db_uuid": "BR", "name": "Brumeister", "division": "Underworld",
                "stats": stats, "effects": [gimmick]},
            "entrance": {"db_uuid": "BR-ent", "name": "ent"}, "cards": cards.clone(),
        }))
        .expect("deck A");
        let deck_b: Deck = serde_json::from_value(json!({
            "competitor": {"db_uuid": "B", "name": "B", "division": "Underworld", "stats": stats},
            "entrance": {"db_uuid": "B-ent", "name": "ent"}, "cards": cards,
        }))
        .expect("deck B");
        let mut engine = Engine::new(
            deck_a,
            deck_b,
            Box::new(FirstLegal),
            1,
            String::new(),
            "sim".into(),
        );
        // Stock every zone the discard-exit paths read: a pile to pull from, a board
        // to be punished, and a hand so the hand/discard swap is not a no-op.
        for side in ["A", "B"] {
            let p = engine.state.players.get_mut(side).unwrap();
            for i in 0..3 {
                p.discard
                    .push(serde_json::from_value(card(&format!("{side}d{i}"))).unwrap());
                p.in_play
                    .push(serde_json::from_value(card(&format!("{side}p{i}"))).unwrap());
                p.hand
                    .push(serde_json::from_value(card(&format!("{side}h{i}"))).unwrap());
            }
        }
        engine
    }

    fn board(engine: &Engine, key: &str) -> usize {
        engine.state.players[key].in_play.len()
    }

    fn any_card() -> CardFilter {
        CardFilter::default()
    }

    #[test]
    fn opponents_recur_to_hand_fires_the_board_wipe() {
        // B pulls a card out of their own discard -> A discards one of B's in-play.
        let mut engine = brumeister_engine();
        engine.act_add_from_discard(&any_card(), "B").unwrap();
        assert_eq!(board(&engine, "B"), 2, "B loses one in-play card");
        assert_eq!(board(&engine, "A"), 3, "A's own board is untouched");
    }

    #[test]
    fn own_discard_move_does_not_fire_the_opp_gated_effect() {
        // A pulling from their OWN pile must not fire A's who=OPP OnDiscardMove.
        let mut engine = brumeister_engine();
        engine.act_add_from_discard(&any_card(), "A").unwrap();
        assert_eq!(board(&engine, "B"), 3, "who=OPP ignores your own pile");
    }

    #[test]
    fn every_effect_driven_exit_fires_it() {
        // Each of the other discard-exit paths on B's pile also counts as a "move".
        for exit in [
            "shuffle_into_deck",
            "recur_to_deck_top",
            "swap_hand_discard",
        ] {
            let mut engine = brumeister_engine();
            match exit {
                "shuffle_into_deck" => engine
                    .act_shuffle_into_deck(
                        &any_card(),
                        ShuffleSource::Discard,
                        false,
                        false,
                        false,
                        "B",
                    )
                    .unwrap(),
                "recur_to_deck_top" => engine.act_recur_to_deck_top(&any_card(), 2, "B").unwrap(),
                _ => engine.act_swap_hand_discard("B").unwrap(),
            }
            assert_eq!(board(&engine, "B"), 2, "{exit} fires OnDiscardMove");
        }
    }

    #[test]
    fn fires_once_per_action_not_per_card() {
        // "moves ANY NUMBER of cards": a 2-card recur is still a single trigger.
        let mut engine = brumeister_engine();
        engine.act_recur_to_deck_top(&any_card(), 2, "B").unwrap();
        assert_eq!(
            board(&engine, "B"),
            2,
            "two cards recurred still discards only one"
        );
    }

    #[test]
    fn passing_does_not_fire_it() {
        // The mechanical pass-and-recycle is not a card effect.
        let mut engine = brumeister_engine();
        engine.do_pass("B").unwrap();
        assert_eq!(board(&engine, "B"), 3, "pass-and-recycle is not an effect");
    }
}

#[cfg(test)]
mod timed_buff_tests {
    use super::*;

    fn card(uuid: &str) -> Value {
        json!({
            "atk_type": "Strike", "db_uuid": uuid, "effects": [], "finish_bonuses": {},
            "name": uuid, "number": 1, "play_order": "Lead", "raw_text": "", "tags": []
        })
    }

    /// A bare two-sided engine; the timed-buff paths are driven directly.
    fn engine() -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let cards: Vec<Value> = (0..6).map(|i| card(&format!("c{i}"))).collect();
        let deck = |u: &str| -> Deck {
            serde_json::from_value(json!({
                "competitor": {"db_uuid": u, "name": u, "division": "World Championship",
                    "stats": stats},
                "entrance": {"db_uuid": format!("{u}-ent"), "name": "ent"}, "cards": cards.clone(),
            }))
            .expect("deck")
        };
        let decider = Box::new(ReplayDecider::new(BTreeMap::new(), BTreeMap::new()));
        Engine::new(
            deck("A"),
            deck("B"),
            decider,
            1,
            String::new(),
            "sim".into(),
        )
    }

    const CLAUSE: &str = "+1 to Strike and +5 to Submission (Max +5 to each)";

    fn grant(engine: &mut Engine, skill: Skill, delta: i64) {
        engine.grant_timed_buff(
            TimedBuff {
                skill,
                delta,
                until: Duration::UntilStartOfYourNextTurn,
                source: CLAUSE.to_owned(),
                cap: Some(5),
                granted_turn: 0,
            },
            Who::SelfSide,
            "A",
        );
    }

    fn buff_total(engine: &Engine, skill: Skill) -> i64 {
        engine.state.players["A"]
            .timed_buffs
            .iter()
            .filter(|b| b.skill == skill)
            .map(|b| b.delta)
            .sum()
    }

    #[test]
    fn repeat_firings_of_one_clause_accumulate_and_cap() {
        // Snake Pitt Super Lucha, hand-adjudicated: each qualifying Power roll adds
        // another +1 Strike / +5 Submission, and "(Max +5 to each)" is the ceiling on
        // the ACCUMULATED total — so Strike climbs 1..5 and stops, Submission caps at
        // once. One entry per (clause, skill), never a growing list.
        let mut engine = engine();
        for expected in 1..=5 {
            grant(&mut engine, Skill::Strike, 1);
            grant(&mut engine, Skill::Submission, 5);
            assert_eq!(buff_total(&engine, Skill::Strike), expected);
            assert_eq!(buff_total(&engine, Skill::Submission), 5, "capped at once");
        }
        grant(&mut engine, Skill::Strike, 1);
        assert_eq!(
            buff_total(&engine, Skill::Strike),
            5,
            "Strike stops at the cap"
        );
        assert_eq!(
            engine.state.players["A"].timed_buffs.len(),
            2,
            "one entry per (clause, skill) — repeats accumulate, never append"
        );
    }

    #[test]
    fn the_buff_feeds_the_derived_stats() {
        let mut engine = engine();
        grant(&mut engine, Skill::Submission, 5);
        assert_eq!(
            engine.stat("A", Skill::Submission),
            10,
            "base 5 + a capped +5 reaches the derived stat"
        );
        assert_eq!(engine.stat("B", Skill::Submission), 5, "B is untouched");
    }

    /// TurnRollBonus (task #131): "Your Power is +2 during turn rolls" adds to the turn
    /// roll only when Power is the rolled skill, and never leaks into the general
    /// derived stat (finish rolls / stops / skill comparisons) the way a BuffSkill would.
    #[test]
    fn turn_roll_bonus_is_skill_gated_and_phase_scoped() {
        let mut engine = engine();
        let card: Card = serde_json::from_value(json!({
            "atk_type": "Strike", "db_uuid": "trb", "name": "trb", "number": 1,
            "play_order": "Lead", "raw_text": "", "tags": [], "finish_bonuses": {},
            "effects": [{"@type": "Effect", "trigger": {"@type": "Static"},
                "condition": {"@type": "Always"},
                "actions": [{"@type": "TurnRollBonus", "skill": "Power", "delta": 2}],
                "duration": "WHILE_IN_PLAY",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "", "source": "card", "optional": false}]
        }))
        .unwrap();
        engine.state.players.get_mut("A").unwrap().in_play = vec![card];
        assert_eq!(
            engine.turn_roll_bonus("A", Skill::Power),
            2,
            "+2 to the turn roll when Power is rolled"
        );
        assert_eq!(
            engine.turn_roll_bonus("A", Skill::Technique),
            0,
            "no bonus when another skill is rolled"
        );
        assert_eq!(
            engine.stat("A", Skill::Power),
            5,
            "phase-scoped: it does NOT leak into the general derived stat"
        );
    }

    /// "Your opponent's Power is -2 during their turn and turn rolls" (task #131): the
    /// opponent-directed two-piece debuff. It sits on A's board but bites B (A's opponent):
    /// (A) a BuffSkill{who:Opp} gated DuringTurn{SELF} — because effective_stats keys the
    /// gate to the buffed side (B), DuringTurn{SELF} reads as "active == B", so B's Power
    /// is -2 on B's own turn and normal on A's turn; (B) a TurnRollBonus{who:Opp} that
    /// reduces B's Power turn roll (via the opponent-board scan) but never A's own roll.
    #[test]
    fn opponent_turn_debuff_bites_the_opponents_turn_and_roll_only() {
        let mut engine = engine();
        let card: Card = serde_json::from_value(json!({
            "atk_type": "Strike", "db_uuid": "otd", "name": "otd", "number": 1,
            "play_order": "Lead", "raw_text": "", "tags": [], "finish_bonuses": {},
            "effects": [
                {"@type": "Effect", "trigger": {"@type": "Static"},
                    "condition": {"@type": "DuringTurn", "who": "SELF"},
                    "actions": [{"@type": "BuffSkill", "skill": "Power", "delta": -2, "who": "OPP",
                        "duration": "WHILE_IN_PLAY", "target_highest": false, "target_lowest": false,
                        "per_crowd": false, "cap": null, "per": null, "per_zone": "IN_PLAY"}],
                    "duration": "WHILE_IN_PLAY",
                    "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                    "raw_clause": "", "source": "card", "optional": false},
                {"@type": "Effect", "trigger": {"@type": "Static"},
                    "condition": {"@type": "Always"},
                    "actions": [{"@type": "TurnRollBonus", "skill": "Power", "delta": -2, "who": "OPP"}],
                    "duration": "WHILE_IN_PLAY",
                    "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                    "raw_clause": "", "source": "card", "optional": false}
            ]
        }))
        .unwrap();
        engine.state.players.get_mut("A").unwrap().in_play = vec![card];
        let base = engine.state.players["B"].competitor.stats.get(Skill::Power);

        // Piece (A): the stat debuff bites B only on B's own turn.
        engine.state.in_turn_roll = false;
        engine.state.active = "B".to_owned();
        assert_eq!(
            engine.stat("B", Skill::Power),
            base - 2,
            "B's Power is reduced on B's turn"
        );
        engine.state.active = "A".to_owned();
        assert_eq!(
            engine.stat("B", Skill::Power),
            base,
            "B's Power is normal on A's turn"
        );

        // Piece (B): the turn-roll debuff reaches B's roll but never A's own.
        assert_eq!(
            engine.turn_roll_bonus("B", Skill::Power),
            -2,
            "reduces the opponent's Power turn roll"
        );
        assert_eq!(
            engine.turn_roll_bonus("B", Skill::Technique),
            0,
            "skill-gated to Power"
        );
        assert_eq!(
            engine.turn_roll_bonus("A", Skill::Power),
            0,
            "does NOT touch the owner's own turn roll"
        );
    }

    /// A per-Crowd-Meter TurnRollBonus ("your Technique is + the Crowd Meter (Max +3)
    /// during your turn roll", task #131): the roll-off delta tracks the LIVE Crowd
    /// Meter clamped to the cap, and — like every TurnRollBonus — never leaks into the
    /// general derived stat.
    #[test]
    fn per_crowd_turn_roll_bonus_tracks_the_crowd_meter_capped() {
        let mut engine = engine();
        let card: Card = serde_json::from_value(json!({
            "atk_type": "Strike", "db_uuid": "pctrb", "name": "pctrb", "number": 1,
            "play_order": "Lead", "raw_text": "", "tags": [], "finish_bonuses": {},
            "effects": [{"@type": "Effect", "trigger": {"@type": "Static"},
                "condition": {"@type": "Always"},
                "actions": [{"@type": "TurnRollBonus", "skill": "Technique",
                             "delta": 1, "per_crowd": true, "cap": 3}],
                "duration": "WHILE_IN_PLAY",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "", "source": "card", "optional": false}]
        }))
        .unwrap();
        engine.state.players.get_mut("A").unwrap().in_play = vec![card];

        engine.state.crowd_meter = 2;
        assert_eq!(
            engine.turn_roll_bonus("A", Skill::Technique),
            2,
            "delta = the live Crowd Meter (2) under the cap"
        );
        engine.state.crowd_meter = 5;
        assert_eq!(
            engine.turn_roll_bonus("A", Skill::Technique),
            3,
            "clamped to the cap (+3) when the Crowd Meter exceeds it"
        );
        assert_eq!(
            engine.stat("A", Skill::Technique),
            engine.state.players["A"]
                .competitor
                .stats
                .get(Skill::Technique),
            "phase-scoped: never leaks into the general derived stat"
        );
    }

    /// A "during your turn" standing buff — a Static self-side BuffSkill gated by
    /// Condition::DuringTurn (task #131). It folds into the derived stat only while it is
    /// the owner's turn (so it reaches your Finish rolls and skill requirements), stays
    /// off during the opponent's turn, and — the fidelity point — is excluded from the
    /// turn roll-off, where `in_turn_roll` is set even though `active` still names the
    /// prior turn's winner. (The "and turn rolls" variant restores the roll via a
    /// separate TurnRollBonus, exercised by the roll-bonus tests above.)
    /// "During your opponent's turn, your Power is +2" (task #131): a self-side BuffSkill
    /// gated on DuringTurn{OPP}. Because the derived-stats gate resolves against the buffed
    /// side (the closure is keyed to A), DuringTurn{OPP} reads as "active == A's opponent",
    /// so the buff is live only on the opponent's turn — reaching the Finish A rolls then —
    /// and is off on A's own turn and in the roll-off.
    #[test]
    fn during_opponent_turn_buff_is_live_only_on_the_opponents_turn() {
        let mut engine = engine();
        let card: Card = serde_json::from_value(json!({
            "atk_type": "Strike", "db_uuid": "dotb", "name": "dotb", "number": 1,
            "play_order": "Lead", "raw_text": "", "tags": [], "finish_bonuses": {},
            "effects": [{"@type": "Effect", "trigger": {"@type": "Static"},
                "condition": {"@type": "DuringTurn", "who": "OPP"},
                "actions": [{"@type": "BuffSkill", "skill": "Power", "delta": 2, "who": "SELF",
                    "duration": "WHILE_IN_PLAY", "target_highest": false, "target_lowest": false,
                    "per_crowd": false, "cap": null, "per": null, "per_zone": "IN_PLAY"}],
                "duration": "WHILE_IN_PLAY",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "", "source": "card", "optional": false}]
        }))
        .unwrap();
        engine.state.players.get_mut("A").unwrap().in_play = vec![card];
        let base = engine.state.players["A"].competitor.stats.get(Skill::Power);

        // The opponent's turn (B active): the buff applies to A.
        engine.state.active = "B".to_owned();
        engine.state.in_turn_roll = false;
        assert_eq!(
            engine.stat("A", Skill::Power),
            base + 2,
            "live on the opponent's turn"
        );

        // A's own turn: off.
        engine.state.active = "A".to_owned();
        assert_eq!(engine.stat("A", Skill::Power), base, "off on your own turn");

        // The roll-off: nobody's turn yet, so off regardless of the stale active seat.
        engine.state.in_turn_roll = true;
        assert_eq!(
            engine.stat("A", Skill::Power),
            base,
            "excluded from the turn roll-off"
        );
    }

    #[test]
    fn during_turn_buff_gates_on_whose_turn_and_skips_the_roll_off() {
        let mut engine = engine();
        let card: Card = serde_json::from_value(json!({
            "atk_type": "Strike", "db_uuid": "dtb", "name": "dtb", "number": 1,
            "play_order": "Lead", "raw_text": "", "tags": [], "finish_bonuses": {},
            "effects": [{"@type": "Effect", "trigger": {"@type": "Static"},
                "condition": {"@type": "DuringTurn", "who": "SELF"},
                "actions": [{"@type": "BuffSkill", "skill": "Power", "delta": 2, "who": "SELF",
                    "duration": "WHILE_IN_PLAY", "target_highest": false, "target_lowest": false,
                    "per_crowd": false, "cap": null, "per": null, "per_zone": "IN_PLAY"}],
                "duration": "WHILE_IN_PLAY",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "", "source": "card", "optional": false}]
        }))
        .unwrap();
        engine.state.players.get_mut("A").unwrap().in_play = vec![card];
        let base = engine.state.players["A"].competitor.stats.get(Skill::Power);

        // It is A's turn: the buff applies.
        engine.state.active = "A".to_owned();
        engine.state.in_turn_roll = false;
        assert_eq!(
            engine.stat("A", Skill::Power),
            base + 2,
            "applies during the owner's turn"
        );

        // The opponent's turn: off.
        engine.state.active = "B".to_owned();
        assert_eq!(
            engine.stat("A", Skill::Power),
            base,
            "off during the opponent's turn"
        );

        // The roll-off: `active` still names A (won last turn) but it is nobody's turn
        // yet — the buff must NOT leak into A's turn roll.
        engine.state.active = "A".to_owned();
        engine.state.in_turn_roll = true;
        assert_eq!(
            engine.stat("A", Skill::Power),
            base,
            "excluded from the turn roll-off despite the stale active seat"
        );
    }

    /// "If you have another Follow Up or Finish Strike in play, your Technique skill
    /// is +1" (task #119/#130 skill-buff family): a Static buff gated on HasInPlay
    /// count>=2 of the play_orders OR-filter. The gate is re-evaluated live off the
    /// board, so a qualifying card landing LATER turns the +1 on; a wrong attack type
    /// never satisfies it.
    #[test]
    fn gated_order_or_buff_activates_when_a_second_qualifier_lands() {
        let fu_strike = |uuid: &str, effects: serde_json::Value| -> Card {
            serde_json::from_value(json!({
                "atk_type": "Strike", "db_uuid": uuid, "name": uuid, "number": 1,
                "play_order": "Followup", "raw_text": "", "tags": [], "finish_bonuses": {},
                "effects": effects
            }))
            .unwrap()
        };
        let gated = json!([{
            "@type": "Effect", "trigger": {"@type": "Static"},
            "condition": {"@type": "HasInPlay", "who": "SELF", "cmp": ">=", "count": 2,
                "filter": {"@type": "CardFilter", "atk_type": "Strike",
                    "play_orders": ["Followup", "Finish"], "is_stop": null, "name": null,
                    "name_contains": [], "number": null, "play_order": null, "raw": null,
                    "tag": null, "text_contains": []}},
            "actions": [{"@type": "BuffSkill", "skill": "Technique", "delta": 1, "who": "SELF",
                "duration": "WHILE_IN_PLAY", "target_highest": false, "target_lowest": false,
                "per_crowd": false, "cap": null, "per": null, "per_zone": "IN_PLAY"}],
            "duration": "WHILE_IN_PLAY",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "", "source": "card", "optional": false
        }]);

        let mut engine = engine();
        // Only the buff card itself (a Follow Up Strike): count 1 < 2, buff OFF.
        engine.state.players.get_mut("A").unwrap().in_play = vec![fu_strike("buffer", gated)];
        assert_eq!(
            engine.stat("A", Skill::Technique),
            5,
            "a lone Follow Up Strike does not satisfy its own 'another' gate"
        );

        // A single FINISH Strike landing later is enough (source + 1 other = count 2):
        // the gate flips true and the standing +1 applies live. One qualifier, not two.
        let finish_strike: Card = serde_json::from_value(json!({
            "atk_type": "Strike", "db_uuid": "fin", "name": "fin", "number": 1,
            "play_order": "Finish", "raw_text": "", "tags": [], "finish_bonuses": {},
            "effects": []
        }))
        .unwrap();
        engine
            .state
            .players
            .get_mut("A")
            .unwrap()
            .in_play
            .push(finish_strike);
        assert_eq!(
            engine.stat("A", Skill::Technique),
            6,
            "one other qualifier (a Finish Strike) turns the standing +1 on"
        );

        // Replace the second card with a Follow Up GRAPPLE: wrong attack type, no gate.
        let grapple: Card = serde_json::from_value(json!({
            "atk_type": "Grapple", "db_uuid": "g", "name": "g", "number": 1,
            "play_order": "Followup", "raw_text": "", "tags": [], "finish_bonuses": {},
            "effects": []
        }))
        .unwrap();
        let ip = &mut engine.state.players.get_mut("A").unwrap().in_play;
        ip.pop();
        ip.push(grapple);
        assert_eq!(
            engine.stat("A", Skill::Technique),
            5,
            "a Follow Up Grapple is the wrong attack type — gate stays false"
        );
    }

    #[test]
    fn until_start_of_your_next_turn_survives_the_granting_turns_roll() {
        // Granted on turn 3's roll: the sweep for turn 3 must NOT take it, or the buff
        // would never survive the turn that created it.
        let mut engine = engine();
        engine.state.turn_no = 3;
        grant(&mut engine, Skill::Submission, 5);
        engine.sweep_next_turn_buffs("A");
        assert_eq!(buff_total(&engine, Skill::Submission), 5);
    }

    #[test]
    fn it_survives_every_turn_its_owner_is_not_active() {
        // Granted turn 3; B wins turns 4 and 5 -> A's buff is untouched throughout.
        let mut engine = engine();
        engine.state.turn_no = 3;
        grant(&mut engine, Skill::Submission, 5);
        for turn in 4..=5 {
            engine.state.turn_no = turn;
            engine.sweep_next_turn_buffs("B");
            assert_eq!(buff_total(&engine, Skill::Submission), 5, "turn {turn}");
        }
        // Turn 6: A wins the roll. The buff fed that roll and is swept right after.
        engine.state.turn_no = 6;
        engine.sweep_next_turn_buffs("A");
        assert_eq!(
            buff_total(&engine, Skill::Submission),
            0,
            "swept after the roll"
        );
    }

    #[test]
    fn until_end_of_turn_is_not_touched_by_the_next_turn_sweep() {
        // The two durations have separate sweeps; the roll-time sweep must ignore
        // UntilEndOfTurn (which is cleared at the top of the following turn instead).
        let mut engine = engine();
        engine.grant_timed_buff(
            TimedBuff {
                skill: Skill::Strike,
                delta: 2,
                until: Duration::UntilEndOfTurn,
                source: "until the end of the turn".to_owned(),
                cap: None,
                granted_turn: 0,
            },
            Who::SelfSide,
            "A",
        );
        engine.state.turn_no = 9;
        engine.sweep_next_turn_buffs("A");
        assert_eq!(
            buff_total(&engine, Skill::Strike),
            2,
            "wrong sweep must not fire"
        );
    }
}

#[cfg(test)]
mod blank_stopped_text_tests {
    use super::*;

    /// A card whose "If Stopped" text draws 2 — the thing the blank must suppress.
    fn attack_with_if_stopped() -> Card {
        serde_json::from_value(json!({
            "atk_type": "Grapple", "db_uuid": "attack", "name": "If Stopped Grapple",
            "number": 5, "play_order": "Lead", "raw_text": "If Stopped, draw 2 cards.",
            "tags": [], "finish_bonuses": {},
            "effects": [{
                "@type": "Effect",
                "trigger": {"@type": "OnStop", "dir": "YOURS", "order": null},
                "condition": {"@type": "Always"},
                "actions": [{"@type": "Draw", "n": 2, "source": "TOP", "who": "SELF",
                             "per": null, "per_who": "SELF"}],
                "duration": "INSTANT",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "If Stopped, draw 2 cards.", "source": "card", "optional": false
            }]
        }))
        .unwrap()
    }

    /// The stop card: "when you stop a card, the stopped card has blank text until the
    /// end of the turn" (`blanks = true`), or an inert stop card (`blanks = false`).
    fn stop_card(blanks: bool) -> Card {
        let effects = if blanks {
            json!([{
                "@type": "Effect",
                "trigger": {"@type": "OnStop", "dir": "THEIRS", "order": null},
                "condition": {"@type": "Always"},
                "actions": [{"@type": "BlankStoppedText"}],
                "duration": "INSTANT",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "the stopped card has blank text until the end of the turn",
                "source": "card", "optional": false
            }])
        } else {
            json!([])
        };
        serde_json::from_value(json!({
            "atk_type": "Grapple", "db_uuid": "stopper", "name": "Blocker", "number": 6,
            "play_order": "Lead", "raw_text": "", "tags": [], "finish_bonuses": {},
            "effects": effects
        }))
        .unwrap()
    }

    fn engine() -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let cards: Vec<Value> = (0..8)
            .map(|i| {
                json!({"atk_type": "Strike", "db_uuid": format!("c{i}"), "effects": [],
                       "finish_bonuses": {}, "name": format!("c{i}"), "number": 1,
                       "play_order": "Lead", "raw_text": "", "tags": []})
            })
            .collect();
        let deck = |u: &str| -> Deck {
            serde_json::from_value(json!({
                "competitor": {"db_uuid": u, "name": u, "division": "World Championship",
                    "stats": stats},
                "entrance": {"db_uuid": format!("{u}-ent"), "name": "ent"}, "cards": cards.clone(),
            }))
            .expect("deck")
        };
        let decider = Box::new(ReplayDecider::new(BTreeMap::new(), BTreeMap::new()));
        Engine::new(
            deck("A"),
            deck("B"),
            decider,
            1,
            String::new(),
            "sim".into(),
        )
    }

    /// A's card is stopped by B; returns how many cards A drew from "If Stopped".
    fn run_stop(blanks: bool) -> (Engine, usize) {
        let mut engine = engine();
        let before = engine.state.players["A"].hand.len();
        engine
            .apply_stop(
                "A",
                "B",
                attack_with_if_stopped(),
                stop_card(blanks),
                vec![],
            )
            .unwrap();
        let drew = engine.state.players["A"].hand.len() - before;
        (engine, drew)
    }

    #[test]
    fn an_unblanked_stop_lets_if_stopped_fire() {
        // Baseline: without the blank, "If Stopped, draw 2" resolves normally.
        let (_, drew) = run_stop(false);
        assert_eq!(drew, 2, "If Stopped fires when nothing blanks it");
    }

    #[test]
    fn blanking_the_stopped_card_suppresses_if_stopped() {
        // The point of the family: the blank lands before the stopped card's own
        // OnStop, so its "If Stopped" text never triggers.
        let (engine, drew) = run_stop(true);
        assert_eq!(drew, 0, "a blanked card's If Stopped must not fire");
        assert!(
            engine.state.blanked_text.contains("attack"),
            "the stopped card is recorded as blanked"
        );
    }

    #[test]
    fn the_blank_lasts_the_rest_of_the_turn_and_is_swept() {
        let (mut engine, _) = run_stop(true);
        let attack = attack_with_if_stopped();
        assert!(
            engine.state.is_text_blanked(&attack, "A"),
            "still blanked later in the same turn"
        );
        engine.sweep_end_of_turn(); // the next turn's per-turn resets sweep it
        assert!(
            !engine.state.is_text_blanked(&attack, "A"),
            "the blank does not outlive the turn"
        );
    }
}

#[cfg(test)]
mod choose_name_tests {
    use super::*;

    /// Always picks the option named by `pick` at a `name` decision point.
    struct PickName(&'static str);

    impl Decider for PickName {
        fn decide(
            &mut self,
            _point: &str,
            _viewer: &str,
            legal: &[Value],
            _state: &mut GameState,
        ) -> Option<Value> {
            legal
                .iter()
                .find(|o| o["name"].as_str() == Some(self.0))
                .cloned()
                .or_else(|| legal.first().cloned())
        }

        fn policy_name(&self, _viewer: &str) -> String {
            "pick-name".to_owned()
        }
    }

    const NAMES: [&str; 3] = ["Kendo Stick", "Steel Chair", "Trash Can"];

    /// Raven's gimmick: bind one name at match start, then one OnHit per option gated
    /// on the binding — exactly one should ever be live.
    fn raven_effects() -> Value {
        let mut effects = vec![json!({
            "@type": "Effect",
            "trigger": {"@type": "StartOfMatch"},
            "condition": {"@type": "Always"},
            "actions": [{"@type": "ChooseName", "options": NAMES}],
            "duration": "INSTANT",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "Choose 1", "source": "gimmick", "optional": false
        })];
        for n in NAMES {
            effects.push(json!({
                "@type": "Effect",
                "trigger": {"@type": "OnHit", "atk_type": null, "name_contains": [n],
                            "text_contains": [], "on_any": false},
                "condition": {"@type": "ChosenNameIs", "name": n, "who": "SELF"},
                "actions": [{"@type": "Draw", "n": 2, "source": "TOP", "who": "SELF",
                             "per": null, "per_who": "SELF"}],
                "duration": "INSTANT",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "draw 2", "source": "gimmick", "optional": false
            }));
        }
        Value::Array(effects)
    }

    fn engine(pick: &'static str) -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let cards: Vec<Value> = (0..10)
            .map(|i| {
                json!({"atk_type": "Strike", "db_uuid": format!("c{i}"), "effects": [],
                       "finish_bonuses": {}, "name": format!("c{i}"), "number": 1,
                       "play_order": "Lead", "raw_text": "", "tags": []})
            })
            .collect();
        let deck_a: Deck = serde_json::from_value(json!({
            "competitor": {"db_uuid": "RV", "name": "Raven", "division": "World Championship",
                "stats": stats, "effects": raven_effects()},
            "entrance": {"db_uuid": "RV-ent", "name": "ent"}, "cards": cards.clone(),
        }))
        .expect("deck A");
        let deck_b: Deck = serde_json::from_value(json!({
            "competitor": {"db_uuid": "B", "name": "B", "division": "World Championship",
                "stats": stats},
            "entrance": {"db_uuid": "B-ent", "name": "ent"}, "cards": cards,
        }))
        .expect("deck B");
        Engine::new(
            deck_a,
            deck_b,
            Box::new(PickName(pick)),
            1,
            String::new(),
            "sim".into(),
        )
    }

    /// Fire A's hit gimmicks against a card named `card_name`; return cards drawn.
    fn hit(engine: &mut Engine, card_name: &str) -> usize {
        let card: Card = serde_json::from_value(json!({
            "atk_type": "Strike", "db_uuid": "hit", "effects": [], "finish_bonuses": {},
            "name": card_name, "number": 1, "play_order": "Lead", "raw_text": "", "tags": []
        }))
        .unwrap();
        let before = engine.state.players["A"].hand.len();
        engine.run_hit_gimmicks(&card, "A").unwrap();
        engine.state.players["A"].hand.len() - before
    }

    #[test]
    fn the_binding_is_recorded_at_match_start() {
        let mut engine = engine("Steel Chair");
        engine.setup().unwrap();
        assert_eq!(
            engine.state.players["A"].chosen_name.as_deref(),
            Some("Steel Chair")
        );
    }

    #[test]
    fn only_the_chosen_name_draws() {
        let mut engine = engine("Steel Chair");
        engine.setup().unwrap();
        assert_eq!(
            hit(&mut engine, "Folding Steel Chair"),
            2,
            "chosen name hits"
        );
        assert_eq!(hit(&mut engine, "Kendo Stick Shot"), 0, "unchosen is inert");
        assert_eq!(hit(&mut engine, "Trash Can Lid"), 0, "unchosen is inert");
        assert_eq!(hit(&mut engine, "Dropkick"), 0, "unrelated card is inert");
    }

    #[test]
    fn a_different_choice_moves_the_live_effect() {
        let mut engine = engine("Trash Can");
        engine.setup().unwrap();
        assert_eq!(hit(&mut engine, "Trash Can Lid"), 2);
        assert_eq!(hit(&mut engine, "Folding Steel Chair"), 0);
    }

    #[test]
    fn nothing_fires_before_a_choice_is_bound() {
        // ChosenNameIs is false while the binding is None, so no OnHit is live.
        let mut engine = engine("Steel Chair");
        assert_eq!(hit(&mut engine, "Folding Steel Chair"), 0);
    }
}

#[cfg(test)]
mod hit_order_and_per_cap_tests {
    use super::*;

    fn lead(uuid: &str) -> Value {
        json!({"atk_type": "Strike", "db_uuid": uuid, "effects": [], "finish_bonuses": {},
               "name": uuid, "number": 1, "play_order": "Lead", "raw_text": "", "tags": []})
    }

    /// Sticky Sailboat: OnHit{order=Lead} -> draw 1 per OTHER Lead in play, max 3.
    fn gimmick() -> Value {
        json!({
            "@type": "Effect",
            "trigger": {"@type": "OnHit", "atk_type": null, "name_contains": [],
                        "text_contains": [], "on_any": false, "order": "Lead"},
            "condition": {"@type": "Always"},
            "actions": [{"@type": "Draw", "n": 1, "source": "TOP", "who": "SELF",
                         "per": {"@type": "CardFilter", "number": null, "atk_type": null,
                                 "play_order": "Lead", "tag": null, "name": null, "raw": null,
                                 "name_contains": [], "text_contains": []},
                         "per_who": "SELF", "cap": 3, "per_excludes_trigger": true}],
            "duration": "INSTANT",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "test", "source": "gimmick", "optional": false
        })
    }

    fn engine() -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let cards: Vec<Value> = (0..20).map(|i| lead(&format!("c{i}"))).collect();
        let deck_a: Deck = serde_json::from_value(json!({
            "competitor": {"db_uuid": "SS", "name": "Sticky", "division": "World Championship",
                "stats": stats, "effects": [gimmick()]},
            "entrance": {"db_uuid": "SS-ent", "name": "ent"}, "cards": cards.clone(),
        }))
        .expect("deck A");
        let deck_b: Deck = serde_json::from_value(json!({
            "competitor": {"db_uuid": "B", "name": "B", "division": "World Championship",
                "stats": stats},
            "entrance": {"db_uuid": "B-ent", "name": "ent"}, "cards": cards,
        }))
        .expect("deck B");
        let decider = Box::new(ReplayDecider::new(BTreeMap::new(), BTreeMap::new()));
        Engine::new(deck_a, deck_b, decider, 1, String::new(), "sim".into())
    }

    /// Put `leads` Leads on A's board, then resolve a hit of `hit` (already in play,
    /// as `run_hit_gimmicks` sees it); return cards drawn.
    fn hit_with(board_leads: usize, hit: Value) -> usize {
        let mut engine = engine();
        {
            let p = engine.state.players.get_mut("A").unwrap();
            for i in 0..board_leads {
                p.in_play
                    .push(serde_json::from_value(lead(&format!("b{i}"))).unwrap());
            }
            p.in_play.push(serde_json::from_value(hit.clone()).unwrap());
        }
        let card: Card = serde_json::from_value(hit).unwrap();
        let before = engine.state.players["A"].hand.len();
        engine.run_hit_gimmicks(&card, "A").unwrap();
        engine.state.players["A"].hand.len() - before
    }

    #[test]
    fn the_triggering_lead_is_excluded_from_its_own_count() {
        // Board = 1 other Lead + the hit Lead. "each OTHER Lead" => 1, not 2.
        assert_eq!(hit_with(1, lead("hit")), 1);
        // No other Leads: the hit card alone must not draw for itself.
        assert_eq!(hit_with(0, lead("hit")), 0);
    }

    #[test]
    fn the_max_clamps_the_per_count() {
        // 5 other Leads would be 5; "(Max 3)" clamps it.
        assert_eq!(hit_with(5, lead("hit")), 3);
        assert_eq!(hit_with(3, lead("hit")), 3, "exactly at the cap");
        assert_eq!(hit_with(2, lead("hit")), 2, "under the cap is untouched");
    }

    #[test]
    fn the_order_gate_ignores_non_leads() {
        // Hitting a Follow Up must not fire an order=Lead gimmick, however many
        // Leads are on the board.
        let mut followup = lead("hit");
        followup["play_order"] = json!("Followup");
        assert_eq!(hit_with(3, followup), 0);
    }
}

#[cfg(test)]
mod choose_target_tests {
    use super::*;

    /// Always takes the legal option whose card uuid starts with `pref`.
    struct PickPrefix(&'static str);

    impl Decider for PickPrefix {
        fn decide(
            &mut self,
            _point: &str,
            _viewer: &str,
            legal: &[Value],
            _state: &mut GameState,
        ) -> Option<Value> {
            legal
                .iter()
                .find(|o| o["card"].as_str().is_some_and(|c| c.starts_with(self.0)))
                .cloned()
                .or_else(|| legal.first().cloned())
        }

        fn policy_name(&self, _viewer: &str) -> String {
            "pick-prefix".to_owned()
        }
    }

    fn card(uuid: &str) -> Value {
        json!({"atk_type": "Strike", "db_uuid": uuid, "effects": [], "finish_bonuses": {},
               "name": uuid, "number": 1, "play_order": "Lead", "raw_text": "", "tags": []})
    }

    /// Both sides get 2 cards in play and 2 in discard, uuid-prefixed by side.
    fn engine(pref: &'static str) -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let cards: Vec<Value> = (0..6).map(|i| card(&format!("d{i}"))).collect();
        let deck = |u: &str| -> Deck {
            serde_json::from_value(json!({
                "competitor": {"db_uuid": u, "name": u, "division": "Underworld", "stats": stats},
                "entrance": {"db_uuid": format!("{u}-ent"), "name": "ent"}, "cards": cards.clone(),
            }))
            .expect("deck")
        };
        let mut engine = Engine::new(
            deck("A"),
            deck("B"),
            Box::new(PickPrefix(pref)),
            1,
            String::new(),
            "sim".into(),
        );
        for side in ["A", "B"] {
            let p = engine.state.players.get_mut(side).unwrap();
            for i in 0..2 {
                p.in_play
                    .push(serde_json::from_value(card(&format!("{side}play{i}"))).unwrap());
                p.discard
                    .push(serde_json::from_value(card(&format!("{side}disc{i}"))).unwrap());
            }
        }
        engine
    }

    fn any() -> CardFilter {
        CardFilter::default()
    }

    fn boards(e: &Engine) -> (usize, usize) {
        (
            e.state.players["A"].in_play.len(),
            e.state.players["B"].in_play.len(),
        )
    }

    #[test]
    fn choose_reaches_the_opponents_board() {
        let mut engine = engine("Bplay");
        engine
            .act_remove_from_play(&any(), Who::SelfSide, 1, true, false, "A")
            .unwrap();
        assert_eq!(boards(&engine), (2, 1), "B lost a card despite who=SELF");
    }

    #[test]
    fn choose_also_reaches_your_own_board() {
        // The hand-adjudicated part: "1 card in play" is not restricted to the
        // opponent, so A may discard its own.
        let mut engine = engine("Aplay");
        engine
            .act_remove_from_play(&any(), Who::Opp, 1, true, false, "A")
            .unwrap();
        assert_eq!(boards(&engine), (1, 2), "A lost a card despite who=OPP");
    }

    #[test]
    fn without_choose_the_who_side_still_decides() {
        // Regression guard: choose=false keeps the original who-directed behaviour.
        let mut engine = engine("Aplay");
        engine
            .act_remove_from_play(&any(), Who::Opp, 1, false, false, "A")
            .unwrap();
        assert_eq!(boards(&engine), (2, 1), "who=OPP still hits B");
    }

    #[test]
    fn to_deck_buries_the_removed_card_to_the_owners_deck() {
        // JT Dunn's "bury it" (to_deck=true): the opponent's in-play card lands on their
        // own deck bottom, not their discard.
        let mut engine = engine("Bplay");
        let deck_before = engine.state.players["B"].deck.len();
        let disc_before = engine.state.players["B"].discard.len();
        engine
            .act_remove_from_play(&any(), Who::Opp, 1, false, true, "A")
            .unwrap();
        assert_eq!(boards(&engine), (2, 1), "B lost an in-play card");
        assert_eq!(
            engine.state.players["B"].deck.len(),
            deck_before + 1,
            "buried to B's deck"
        );
        assert_eq!(
            engine.state.players["B"].discard.len(),
            disc_before,
            "NOT sent to B's discard"
        );
    }

    #[test]
    fn a_chosen_bury_takes_the_named_card_from_either_pile() {
        // "bury 1 card in any player's discard pile" picks a SPECIFIC card (not the
        // top) and returns it to that card's OWNER's deck bottom.
        let mut engine = engine("Bdisc1");
        let spec = BurySpec {
            selector: any(),
            count: 1,
            who: Who::SelfSide,
            random: false,
            source: BuryFrom::Discard,
            choose: true,
        };
        engine.act_bury(spec, "A").unwrap();
        let b = &engine.state.players["B"];
        assert!(
            !b.discard.iter().any(|c| c.db_uuid == "Bdisc1"),
            "the chosen card left B's pile"
        );
        assert_eq!(
            b.deck.last().map(|c| c.db_uuid.as_str()),
            Some("Bdisc1"),
            "and landed on ITS OWNER's deck bottom"
        );
        assert_eq!(
            engine.state.players["A"].discard.len(),
            2,
            "A's pile intact"
        );
    }

    #[test]
    fn without_choose_the_pool_is_only_the_who_sides_pile() {
        // A discard pile has no meaningful order, so the bury is always a CHOICE;
        // `choose` only widens the pool ACROSS piles. Here the decider asks for
        // "Bdisc1", which is not offered, so it falls back within A's own pile.
        let mut engine = engine("Bdisc1");
        let spec = BurySpec {
            selector: any(),
            count: 1,
            who: Who::SelfSide,
            random: false,
            source: BuryFrom::Discard,
            choose: false,
        };
        engine.act_bury(spec, "A").unwrap();
        assert_eq!(
            engine.state.players["B"].discard.len(),
            2,
            "B's pile untouched"
        );
        assert_eq!(
            engine.state.players["A"].discard.len(),
            1,
            "A buried its own"
        );
    }

    #[test]
    fn the_actor_picks_any_card_in_the_pile_not_the_top() {
        // "Bury" selects ANY card in the pile — pile order is not meaningful.
        let mut engine = engine("Adisc1"); // ask for the SECOND card
        let spec = BurySpec {
            selector: any(),
            count: 1,
            who: Who::SelfSide,
            random: false,
            source: BuryFrom::Discard,
            choose: false,
        };
        engine.act_bury(spec, "A").unwrap();
        let a = &engine.state.players["A"];
        assert_eq!(
            a.deck.last().map(|c| c.db_uuid.as_str()),
            Some("Adisc1"),
            "the CHOSEN card was buried, not the top one"
        );
        assert!(
            a.discard.iter().any(|c| c.db_uuid == "Adisc0"),
            "the top card stays"
        );
    }
}

#[cfg(test)]
mod roll_order_tests {
    use super::*;

    // srgpc.net: "If two gimmicks would both trigger during a turn roll, the player
    // with the higher turn roll must resolve their effect first."

    #[test]
    fn the_higher_roll_resolves_first() {
        assert_eq!(Engine::roll_order(3, 9), ["B", "A"]);
        assert_eq!(Engine::roll_order(9, 3), ["A", "B"]);
    }

    #[test]
    fn a_tie_keeps_a_stable_order() {
        // The rules leave an exact tie undefined (and a tie bumps), so the order must
        // at least stay deterministic for replay.
        assert_eq!(Engine::roll_order(7, 7), ["A", "B"]);
        assert_eq!(Engine::roll_order(0, 0), ["A", "B"]);
    }

    #[test]
    fn ordering_is_by_value_not_player_identity() {
        // Guards the actual bug: the roll-off used to be hardcoded A-then-B.
        for (va, vb) in [(1, 2), (5, 6), (0, 10)] {
            assert_eq!(
                Engine::roll_order(va, vb),
                ["B", "A"],
                "B rolled higher ({vb} > {va}) so B resolves first"
            );
        }
    }
}

#[cfg(test)]
mod pending_text_tests {
    use super::*;

    /// The Madness injection: "If stopped, you lose the match via disqualification."
    fn dq_if_stopped() -> Value {
        json!({
            "@type": "Effect",
            "trigger": {"@type": "OnStop", "dir": "YOURS", "order": null},
            "condition": {"@type": "Always"},
            "actions": [{"@type": "LoseBy", "kind": "DISQUALIFICATION", "who": "SELF"}],
            "duration": "INSTANT",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "If stopped, you lose via DQ.", "source": "card", "optional": false
        })
    }

    fn card(uuid: &str, atk: &str) -> Value {
        json!({"atk_type": atk, "db_uuid": uuid, "effects": [], "finish_bonuses": {},
               "name": uuid, "number": 1, "play_order": "Lead", "raw_text": "", "tags": []})
    }

    fn engine() -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let cards: Vec<Value> = (0..6).map(|i| card(&format!("c{i}"), "Strike")).collect();
        let deck = |u: &str| -> Deck {
            serde_json::from_value(json!({
                "competitor": {"db_uuid": u, "name": u, "division": "Underworld", "stats": stats},
                "entrance": {"db_uuid": format!("{u}-ent"), "name": "ent"}, "cards": cards.clone(),
            }))
            .expect("deck")
        };
        let decider = Box::new(ReplayDecider::new(BTreeMap::new(), BTreeMap::new()));
        Engine::new(
            deck("A"),
            deck("B"),
            decider,
            1,
            String::new(),
            "sim".into(),
        )
    }

    /// Queue "next Grapple gains DQ-if-stopped" on B.
    fn poison(engine: &mut Engine) {
        let selector: CardFilter = serde_json::from_value(json!({
            "@type": "CardFilter", "number": null, "atk_type": "Grapple", "play_order": null,
            "tag": null, "name": null, "raw": null, "name_contains": [], "text_contains": []
        }))
        .unwrap();
        let effects: Vec<Effect> = vec![serde_json::from_value(dq_if_stopped()).unwrap()];
        engine.act_add_text_to_next(Who::Opp, &selector, &effects, "A");
    }

    fn pending(engine: &Engine, key: &str) -> usize {
        engine.state.players[key].pending_text.len()
    }

    #[test]
    fn the_poison_lands_on_the_target_not_the_source() {
        let mut engine = engine();
        poison(&mut engine);
        assert_eq!(pending(&engine, "B"), 1, "queued on the OPPONENT");
        assert_eq!(pending(&engine, "A"), 0, "not on the caster");
    }

    #[test]
    fn only_a_matching_card_consumes_it() {
        let mut engine = engine();
        poison(&mut engine);
        // A Strike does not match the Grapple selector: untouched.
        let mut strike: Card = serde_json::from_value(card("s", "Strike")).unwrap();
        engine.apply_pending_text("B", &mut strike);
        assert!(strike.effects.is_empty(), "non-matching card gains nothing");
        assert_eq!(pending(&engine, "B"), 1, "and the poison is still queued");
        // The next Grapple takes it.
        let mut grapple: Card = serde_json::from_value(card("g", "Grapple")).unwrap();
        engine.apply_pending_text("B", &mut grapple);
        assert_eq!(
            grapple.effects.len(),
            1,
            "the Grapple gained the added text"
        );
        assert_eq!(pending(&engine, "B"), 0, "and it is consumed");
    }

    #[test]
    fn it_is_one_shot() {
        let mut engine = engine();
        poison(&mut engine);
        for uuid in ["g1", "g2"] {
            let mut g: Card = serde_json::from_value(card(uuid, "Grapple")).unwrap();
            engine.apply_pending_text("B", &mut g);
            let expected = usize::from(uuid == "g1");
            assert_eq!(
                g.effects.len(),
                expected,
                "{uuid} only the FIRST Grapple is hit"
            );
        }
    }

    #[test]
    fn the_added_text_survives_its_source_leaving_the_board() {
        // srgpc: poison "stays active until fulfilled even if removed from the board".
        // The queue lives on the target, so clearing BOTH boards changes nothing.
        let mut engine = engine();
        poison(&mut engine);
        for side in ["A", "B"] {
            engine.state.players.get_mut(side).unwrap().in_play.clear();
            engine.state.players.get_mut(side).unwrap().discard.clear();
        }
        let mut g: Card = serde_json::from_value(card("g", "Grapple")).unwrap();
        engine.apply_pending_text("B", &mut g);
        assert_eq!(g.effects.len(), 1, "the poison still resolves");
    }

    #[test]
    fn a_stopped_poisoned_card_disqualifies_its_controller() {
        // End to end: B plays the poisoned Grapple, A stops it, B loses via DQ.
        let mut engine = engine();
        poison(&mut engine);
        let mut g: Card = serde_json::from_value(card("g", "Grapple")).unwrap();
        engine.apply_pending_text("B", &mut g);
        let stop: Card = serde_json::from_value(card("st", "Grapple")).unwrap();
        engine.apply_stop("B", "A", g, stop, vec![]).unwrap();
        engine.resolve_pending();
        let result = engine.result.as_ref().expect("the match ended");
        assert_eq!(result.winner, "A", "the poisoned player's opponent wins");
        assert!(
            result.reason.to_lowercase().contains("disqualification"),
            "by disqualification, got {:?}",
            result.reason
        );
    }
}

#[cfg(test)]
mod timed_blank_tests {
    use super::*;

    fn engine() -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let cards: Vec<Value> = (0..4)
            .map(|i| {
                json!({"atk_type": "Strike", "db_uuid": format!("c{i}"), "effects": [],
                       "finish_bonuses": {}, "name": format!("c{i}"), "number": 1,
                       "play_order": "Lead", "raw_text": "", "tags": []})
            })
            .collect();
        let deck = |u: &str| -> Deck {
            serde_json::from_value(json!({
                "competitor": {"db_uuid": u, "name": u, "division": "Underworld", "stats": stats},
                "entrance": {"db_uuid": format!("{u}-ent"), "name": "ent"}, "cards": cards.clone(),
            }))
            .expect("deck")
        };
        let decider = Box::new(ReplayDecider::new(BTreeMap::new(), BTreeMap::new()));
        Engine::new(
            deck("A"),
            deck("B"),
            decider,
            1,
            String::new(),
            "sim".into(),
        )
    }

    fn lead_card(uuid: &str) -> Card {
        serde_json::from_value(json!({"atk_type": "Strike", "db_uuid": uuid, "effects": [],
            "finish_bonuses": {}, "name": uuid, "number": 1, "play_order": "Lead",
            "raw_text": "", "tags": []}))
        .expect("card deserializes")
    }

    /// A plays Stiff Right Hand on turn 3: B's gimmick is blanked until B's next turn.
    fn blanked_on_turn_3() -> Engine {
        let mut engine = engine();
        engine.state.turn_no = 3;
        engine.act_blank_gimmick(Who::Opp, Duration::UntilStartOfYourNextTurn, "A");
        engine
    }

    #[test]
    fn the_blank_lands_on_the_opponent() {
        let engine = blanked_on_turn_3();
        assert!(engine.state.is_gimmick_blanked("B"), "B is blanked");
        assert!(!engine.state.is_gimmick_blanked("A"), "the caster is not");
    }

    #[test]
    fn it_survives_the_casters_turns_in_between() {
        // "Player A could have won 5 turns in between" — the blank waits for B's turn.
        let mut engine = blanked_on_turn_3();
        for turn in 4..=8 {
            engine.state.turn_no = turn;
            engine.sweep_next_turn_buffs("A"); // A keeps winning
            assert!(
                engine.state.is_gimmick_blanked("B"),
                "still blanked on turn {turn}"
            );
        }
        // B finally wins a turn roll: the blank ends at the start of THEIR turn.
        engine.state.turn_no = 9;
        engine.sweep_next_turn_buffs("B");
        assert!(!engine.state.is_gimmick_blanked("B"), "cleared on B's turn");
    }

    #[test]
    fn the_granting_turns_own_roll_does_not_clear_it() {
        // Granted on turn 3; if B also won turn 3's roll the blank must still apply.
        let mut engine = blanked_on_turn_3();
        engine.sweep_next_turn_buffs("B");
        assert!(engine.state.is_gimmick_blanked("B"));
    }

    #[test]
    fn an_untimed_blank_is_left_alone_by_the_sweep() {
        // Regression guard: a one-shot / StartOfMatch blank has no expiry marker and
        // must not be cleared by the turn boundary.
        let mut engine = engine();
        engine.act_blank_gimmick(Who::Opp, Duration::Instant, "A");
        assert!(engine.state.players["B"].blank_until_next_turn.is_none());
        engine.state.turn_no = 7;
        engine.sweep_next_turn_buffs("B");
        assert!(
            engine.state.is_gimmick_blanked("B"),
            "permanent blank persists"
        );
    }

    #[test]
    fn blank_until_hit_survives_turns_then_lifts_on_a_hit() {
        // Sleep Paralysis: "your opponent's Gimmick is blank until they hit a card."
        let mut engine = engine();
        engine.act_blank_gimmick(Who::Opp, Duration::UntilTargetHitsCard, "A");
        assert!(
            engine.state.players["B"].blank_until_hit,
            "poison latched on B"
        );
        assert!(engine.state.is_gimmick_blanked("B"), "B is blanked");
        // It is NOT a next-turn blank: the turn-boundary sweep leaves it alone.
        engine.state.turn_no = 5;
        engine.sweep_next_turn_buffs("B");
        assert!(
            engine.state.is_gimmick_blanked("B"),
            "still blanked — only a hit lifts it"
        );
        // B lands a hit -> the blank lifts immediately.
        engine.record_landed_hit("B", &lead_card("hit"));
        assert!(!engine.state.players["B"].blank_until_hit, "poison cleared");
        assert!(
            !engine.state.is_gimmick_blanked("B"),
            "gimmick restored once B hits"
        );
    }

    #[test]
    fn reveal_whole_hand_exposes_every_card() {
        // Bermuda Triangle: "Reveal your hand to your opponent."
        let mut engine = engine();
        let hand: Vec<Card> = ["r1", "r2", "r3"].iter().map(|u| lead_card(u)).collect();
        engine.state.players.get_mut("A").unwrap().hand = hand;
        engine
            .act_reveal(Who::SelfSide, 0, true, "A")
            .expect("reveal");
        let a = &engine.state.players["A"];
        for u in ["r1", "r2", "r3"] {
            assert!(
                a.revealed_hand.contains(u),
                "{u} is now known to the opponent"
            );
        }
    }
}

/// `SuppressSelfHandLoss` (task #79 / Sami Callihan): "you do not bury or discard
/// cards from your hand for your OWN card effects" — the two hand-loss chokepoints,
/// and the start-of-match choice (Sami WR) that selects between this flag and
/// `SuppressOpponentDraw`.
#[cfg(test)]
mod suppress_hand_loss_tests {
    use super::*;

    /// Picks the option named `pick` at a `name` decision point; first legal elsewhere.
    struct PickName(&'static str);

    impl Decider for PickName {
        fn decide(
            &mut self,
            _point: &str,
            _viewer: &str,
            legal: &[Value],
            _state: &mut GameState,
        ) -> Option<Value> {
            legal
                .iter()
                .find(|o| o["name"].as_str() == Some(self.0))
                .cloned()
                .or_else(|| legal.first().cloned())
        }

        fn policy_name(&self, _viewer: &str) -> String {
            "pick-name".to_owned()
        }
    }

    const DRAW_OPT: &str = "No Opponent Draw";
    const HAND_OPT: &str = "No Self Hand Loss";

    /// The bare Sami V2 declaration: one unconditional Static flag.
    fn v2_effects() -> Value {
        json!([{
            "@type": "Effect",
            "trigger": {"@type": "Static"},
            "condition": {"@type": "Always"},
            "actions": [{"@type": "SuppressSelfHandLoss"}],
            "duration": "WHILE_GIMMICK_ACTIVE",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "no self hand loss", "source": "gimmick", "optional": false
        }])
    }

    /// Sami WR: bind one option at match start, then one Static per option gated on
    /// the binding — exactly one flag is ever live.
    fn wr_effects() -> Value {
        json!([
            {
                "@type": "Effect",
                "trigger": {"@type": "StartOfMatch"},
                "condition": {"@type": "Always"},
                "actions": [{"@type": "ChooseName", "options": [DRAW_OPT, HAND_OPT]}],
                "duration": "INSTANT",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "choose 1", "source": "gimmick", "optional": false
            },
            {
                "@type": "Effect",
                "trigger": {"@type": "Static"},
                "condition": {"@type": "ChosenNameIs", "name": DRAW_OPT, "who": "SELF"},
                "actions": [{"@type": "SuppressOpponentDraw"}],
                "duration": "WHILE_GIMMICK_ACTIVE",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "no opp draw", "source": "gimmick", "optional": false
            },
            {
                "@type": "Effect",
                "trigger": {"@type": "Static"},
                "condition": {"@type": "ChosenNameIs", "name": HAND_OPT, "who": "SELF"},
                "actions": [{"@type": "SuppressSelfHandLoss"}],
                "duration": "WHILE_GIMMICK_ACTIVE",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "no self hand loss", "source": "gimmick", "optional": false
            }
        ])
    }

    fn engine_with(effects: Value, pick: &'static str) -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let cards: Vec<Value> = (0..10)
            .map(|i| {
                json!({"atk_type": "Strike", "db_uuid": format!("c{i}"), "effects": [],
                       "finish_bonuses": {}, "name": format!("c{i}"), "number": 1,
                       "play_order": "Lead", "raw_text": "", "tags": []})
            })
            .collect();
        let deck_a: Deck = serde_json::from_value(json!({
            "competitor": {"db_uuid": "SC", "name": "Sami", "division": "World Championship",
                "stats": stats, "effects": effects},
            "entrance": {"db_uuid": "SC-ent", "name": "ent"}, "cards": cards.clone(),
        }))
        .expect("deck A");
        let deck_b: Deck = serde_json::from_value(json!({
            "competitor": {"db_uuid": "B", "name": "B", "division": "World Championship",
                "stats": stats},
            "entrance": {"db_uuid": "B-ent", "name": "ent"}, "cards": cards,
        }))
        .expect("deck B");
        Engine::new(
            deck_a,
            deck_b,
            Box::new(PickName(pick)),
            1,
            String::new(),
            "sim".into(),
        )
    }

    /// A discard-1-from-your-own-hand action, as a card effect owned by `key`.
    fn discard_self() -> Action {
        Action::Discard {
            selector: CardFilter::default(),
            count: 1,
            who: Who::SelfSide,
            random: true,
            per: None,
            per_who: Who::SelfSide,
            choose: false,
            all: false,
        }
    }

    fn hand_len(engine: &Engine, key: &str) -> usize {
        engine.state.players[key].hand.len()
    }

    #[test]
    fn your_own_effect_no_longer_costs_you_a_card() {
        let mut engine = engine_with(v2_effects(), DRAW_OPT);
        engine.setup().unwrap();
        let before = hand_len(&engine, "A");
        engine.apply_action(&discard_self(), "A", "").unwrap();
        assert_eq!(hand_len(&engine, "A"), before, "self-discard suppressed");
    }

    #[test]
    fn the_opponents_effect_still_takes_the_card() {
        // "for your OWN card effects" — B's effect making A discard is untouched.
        let mut engine = engine_with(v2_effects(), DRAW_OPT);
        engine.setup().unwrap();
        let before = hand_len(&engine, "A");
        let opp_discard = Action::Discard {
            selector: CardFilter::default(),
            count: 1,
            who: Who::Opp,
            random: true,
            per: None,
            per_who: Who::SelfSide,
            choose: false,
            all: false,
        };
        engine.apply_action(&opp_discard, "B", "").unwrap();
        assert_eq!(
            hand_len(&engine, "A"),
            before - 1,
            "opponent's effect lands"
        );
    }

    #[test]
    fn it_covers_hand_bury_as_well_as_discard() {
        // The declaration reads "bury OR discard", so both chokepoints are voided.
        let mut engine = engine_with(v2_effects(), DRAW_OPT);
        engine.setup().unwrap();
        let bury = Action::Bury {
            selector: CardFilter::default(),
            count: 1,
            who: Who::SelfSide,
            random: true,
            source: BuryFrom::Hand,
            choose: false,
            per: None,
            per_who: Who::SelfSide,
            per_zone: CountZone::InPlay,
            all: false,
        };
        let before = hand_len(&engine, "A");
        engine.apply_action(&bury, "A", "").unwrap();
        assert_eq!(hand_len(&engine, "A"), before, "self hand-bury suppressed");
    }

    #[test]
    fn without_the_flag_the_discard_lands() {
        // Baseline: the same action against a competitor with no declaration.
        let mut engine = engine_with(json!([]), DRAW_OPT);
        engine.setup().unwrap();
        let before = hand_len(&engine, "A");
        engine.apply_action(&discard_self(), "A", "").unwrap();
        assert_eq!(hand_len(&engine, "A"), before - 1);
    }

    #[test]
    fn the_wr_choice_binds_exactly_one_flag() {
        // Picking the hand branch suppresses A's self-discard but leaves A's
        // Draw(who=OPP) working; picking the draw branch does the reverse.
        let mut hand_pick = engine_with(wr_effects(), HAND_OPT);
        hand_pick.setup().unwrap();
        assert!(hand_pick.suppresses_self_hand_loss("A", "A"));
        assert!(!hand_pick.suppresses_opp_draw("A"));

        let mut draw_pick = engine_with(wr_effects(), DRAW_OPT);
        draw_pick.setup().unwrap();
        assert!(!draw_pick.suppresses_self_hand_loss("A", "A"));
        assert!(draw_pick.suppresses_opp_draw("A"));
    }

    #[test]
    fn the_flag_never_protects_the_opponent() {
        // Owner-scoped: A holding it must not stop A's effect from making B discard —
        // that is the whole point of the OTHER branch existing.
        let mut engine = engine_with(v2_effects(), DRAW_OPT);
        engine.setup().unwrap();
        assert!(!engine.suppresses_self_hand_loss("A", "B"));
    }
}

/// Disqualification rules (task #79 / Deathmatch King Matt Cardona, Boatswain):
/// scope semantics, and the 2026-07-20 adjudication that a BLANKED gimmick declares
/// no immunity — the same rule the suppression flags and `ConsideredCompare` follow.
#[cfg(test)]
mod dq_immunity_tests {
    use super::*;

    /// A Static no-DQ declaration at `scope`.
    fn no_dq(scope: &str) -> Value {
        json!([{
            "@type": "Effect",
            "trigger": {"@type": "Static"},
            "condition": {"@type": "Always"},
            "actions": [{"@type": "DisqualificationRule", "enabled": false, "scope": scope}],
            "duration": "WHILE_GIMMICK_ACTIVE",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "you cannot be disqualified", "source": "gimmick", "optional": false
        }])
    }

    /// Engine where A's gimmick carries `a_effects` and B's carries `b_effects`.
    fn engine_with(a_effects: Value, b_effects: Value) -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let cards: Vec<Value> = (0..10)
            .map(|i| {
                json!({"atk_type": "Strike", "db_uuid": format!("c{i}"), "effects": [],
                       "finish_bonuses": {}, "name": format!("c{i}"), "number": 1,
                       "play_order": "Lead", "raw_text": "", "tags": []})
            })
            .collect();
        let deck = |id: &str, effects: Value| -> Deck {
            serde_json::from_value(json!({
                "competitor": {"db_uuid": id, "name": id, "division": "World Championship",
                    "stats": stats, "effects": effects},
                "entrance": {"db_uuid": format!("{id}-ent"), "name": "ent"}, "cards": cards.clone(),
            }))
            .expect("deck")
        };
        Engine::new(
            deck("A", a_effects),
            deck("B", b_effects),
            Box::new(FirstLegal),
            1,
            String::new(),
            "sim".into(),
        )
    }

    /// Always takes the first legal option (no decision points are exercised here).
    struct FirstLegal;

    impl Decider for FirstLegal {
        fn decide(
            &mut self,
            _point: &str,
            _viewer: &str,
            legal: &[Value],
            _state: &mut GameState,
        ) -> Option<Value> {
            legal.first().cloned()
        }

        fn policy_name(&self, _viewer: &str) -> String {
            "first-legal".to_owned()
        }
    }

    #[test]
    fn a_self_scoped_rule_protects_only_its_owner() {
        let engine = engine_with(no_dq("SELF"), json!([]));
        assert!(engine.is_dq_immune("A"));
        assert!(
            !engine.is_dq_immune("B"),
            "SELF must not cover the opponent"
        );
    }

    #[test]
    fn a_match_scoped_rule_protects_both_players() {
        // "This match has no disqualifications" reaches everyone, whoever declares it.
        let engine = engine_with(json!([]), no_dq("MATCH"));
        assert!(engine.is_dq_immune("A"));
        assert!(engine.is_dq_immune("B"));
    }

    #[test]
    fn match_has_no_dq_needs_both_sides_immune() {
        // MatchHasNoDisqualifications (Cardona's Pizza Cutter): true only when NEITHER
        // player can be DQ'd. A lone SelfSide gimmick (Cardona's own) is not enough.
        let self_only = engine_with(no_dq("SELF"), json!([]));
        assert!(
            !self_only.state.match_has_no_dq(),
            "one side's SelfSide immunity does not make it a No-DQ match"
        );
        let match_wide = engine_with(json!([]), no_dq("MATCH"));
        assert!(
            match_wide.state.match_has_no_dq(),
            "a Match-scoped rule immunizes both → No-DQ match"
        );
    }

    #[test]
    fn crowd_meter_increase_fires_the_gimmick() {
        // Khloe Mai's engine: "When the Crowd Meter increases, draw 1." A's
        // OnCrowdMeterIncrease gimmick fires on any positive swing (here an explicit
        // act_crowd(+1)); a DECREASE never fires it; B has no such gimmick.
        let on_cm_draw = json!([{
            "@type": "Effect",
            "trigger": {"@type": "OnCrowdMeterIncrease"},
            "condition": {"@type": "Always"},
            "actions": [{"@type": "Draw", "n": 1, "source": "TOP", "who": "SELF",
                         "per": null, "per_who": "SELF", "cap": null,
                         "per_excludes_trigger": false, "from_crowd": false}],
            "duration": "INSTANT",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "when the Crowd Meter increases, draw 1", "source": "gimmick",
            "optional": false
        }]);
        let mut e = engine_with(on_cm_draw, json!([]));
        let before = e.state.players["A"].hand.len();
        e.act_crowd(1, "A").unwrap();
        assert_eq!(
            e.state.players["A"].hand.len(),
            before + 1,
            "a CM increase fires A's OnCrowdMeterIncrease gimmick (draw 1)"
        );
        let mid = e.state.players["A"].hand.len();
        e.act_crowd(-1, "A").unwrap();
        assert_eq!(
            e.state.players["A"].hand.len(),
            mid,
            "a CM decrease never fires the increase trigger"
        );
    }

    #[test]
    fn bleeding_out_poison_forces_random_discard_moves_for_the_opponent() {
        // #30 Bleeding Out sits in B's discard declaring a Static WHILE_IN_DISCARD
        // ForceRandomDiscardMove{OPP}: A (B's opponent) must resolve discard moves
        // randomly; B, the declaring owner, is unaffected.
        let mut e = engine_with(json!([]), json!([]));
        let poison: Card = serde_json::from_value(json!({
            "atk_type": "Submission", "db_uuid": "bleeding-out", "name": "Bleeding Out",
            "number": 30, "play_order": "Finish", "raw_text": "", "tags": [],
            "finish_bonuses": {},
            "effects": [{
                "@type": "Effect",
                "trigger": {"@type": "Static"},
                "condition": {"@type": "Always"},
                "actions": [{"@type": "ForceRandomDiscardMove", "who": "OPP"}],
                "duration": "WHILE_IN_DISCARD",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "force random", "source": "card", "optional": false
            }]
        }))
        .unwrap();
        assert!(
            !e.state.force_random_discard_move("A"),
            "no poison in play yet"
        );
        e.state.players.get_mut("B").unwrap().discard.push(poison);
        assert!(
            e.state.force_random_discard_move("A"),
            "B's in-discard poison forces A's discard moves random"
        );
        assert!(
            !e.state.force_random_discard_move("B"),
            "the declaring owner (B) is not poisoned"
        );
    }

    #[test]
    fn split_personality_locks_the_owners_discard() {
        // #29 Split Personality sits in B's discard declaring a Static WHILE_IN_DISCARD
        // LockDiscard{SELF}: A cannot move cards OUT of B's discard, but B still can, and
        // A's own discard is not locked.
        let mut e = engine_with(json!([]), json!([]));
        let lock: Card = serde_json::from_value(json!({
            "atk_type": "Grapple", "db_uuid": "split-personality", "name": "Split Personality",
            "number": 29, "play_order": "Finish", "raw_text": "", "tags": [],
            "finish_bonuses": {},
            "effects": [{
                "@type": "Effect",
                "trigger": {"@type": "Static"},
                "condition": {"@type": "Always"},
                "actions": [{"@type": "LockDiscard", "who": "SELF"}],
                "duration": "WHILE_IN_DISCARD",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "lock", "source": "card", "optional": false
            }]
        }))
        .unwrap();
        assert!(
            !e.state.discard_move_locked("B", "A"),
            "no lock in play yet"
        );
        e.state.players.get_mut("B").unwrap().discard.push(lock);
        assert!(
            e.state.discard_move_locked("B", "A"),
            "A cannot move cards out of B's locked discard"
        );
        assert!(
            !e.state.discard_move_locked("B", "B"),
            "B can always move its OWN discard"
        );
        assert!(
            !e.state.discard_move_locked("A", "B"),
            "A's discard carries no lock"
        );
    }

    #[test]
    fn lose_two_turn_rolls_in_a_row_recurs_the_discard_card() {
        // Me Against the World: a WhileInDiscard OnLoseTurn recur gated on
        // LostTurnRollsInARow{2}. `run_on_lose_turn` fires it from the discard (self_card
        // bound); the streak counter gates it. Non-optional here to keep it policy-free.
        let mut e = engine_with(json!([]), json!([]));
        let meaw: Card = serde_json::from_value(json!({
            "atk_type": "Submission", "db_uuid": "meaw", "name": "Me Against the World",
            "number": 30, "play_order": "Finish", "raw_text": "", "tags": [], "finish_bonuses": {},
            "effects": [{
                "@type": "Effect",
                "trigger": {"@type": "OnLoseTurn", "by": null},
                "condition": {"@type": "LostTurnRollsInARow", "who": "SELF", "at_least": 2},
                "actions": [{"@type": "AddSelfToHand"}],
                "duration": "WHILE_IN_DISCARD",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "recur", "source": "card", "optional": false
            }]
        }))
        .unwrap();
        e.state.players.get_mut("A").unwrap().discard.push(meaw);
        // A streak of 1 does not satisfy the gate -> the card stays in the discard.
        e.state.players.get_mut("A").unwrap().turn_losses_in_a_row = 1;
        e.run_on_lose_turn("A").unwrap();
        assert!(
            e.state.players["A"]
                .hand
                .iter()
                .all(|c| c.db_uuid != "meaw"),
            "a 1-loss streak does not recur"
        );
        // Two in a row -> the recur fires and adds the card back to the hand.
        e.state.players.get_mut("A").unwrap().turn_losses_in_a_row = 2;
        e.run_on_lose_turn("A").unwrap();
        assert!(
            e.state.players["A"]
                .hand
                .iter()
                .any(|c| c.db_uuid == "meaw"),
            "two losses in a row recur the card to hand"
        );
        assert!(
            e.state.players["A"]
                .discard
                .iter()
                .all(|c| c.db_uuid != "meaw"),
            "the recurred card left the discard"
        );
    }

    #[test]
    fn grant_breakout_bonus_can_target_the_opponent() {
        // #30 Why So Serious?!?: "your opponent's breakout rolls are -1" grants the penalty
        // to the OPPONENT's timed breakout store, not the actor's.
        let mut e = engine_with(json!([]), json!([]));
        let grant = Action::GrantBreakoutBonus {
            delta: -1,
            who: Who::Opp,
        };
        e.apply_action(&grant, "A", "").unwrap();
        assert_eq!(
            e.state.players["B"].breakout_bonus_eot, -1,
            "the -1 lands on A's opponent (B)"
        );
        assert_eq!(
            e.state.players["A"].breakout_bonus_eot, 0,
            "the actor's own store is untouched"
        );
    }

    #[test]
    fn a_blanked_gimmick_declares_no_immunity() {
        // The 2026-07-20 call: blanking a gimmick makes its text inert, so Cardona's
        // "you cannot be disqualified" dies with it — matching the suppression flags.
        let mut engine = engine_with(no_dq("SELF"), json!([]));
        assert!(engine.is_dq_immune("A"), "active before the blank");
        engine.state.players.get_mut("A").unwrap().gimmick_blanked = true;
        assert!(!engine.is_dq_immune("A"), "blanked gimmick grants nothing");
    }

    #[test]
    fn blanking_one_side_leaves_the_others_match_rule_standing() {
        // A match-scoped rule on B still covers A after A's own gimmick is blanked —
        // the blank silences the DECLARER, not the beneficiary.
        let mut engine = engine_with(no_dq("SELF"), no_dq("MATCH"));
        engine.state.players.get_mut("A").unwrap().gimmick_blanked = true;
        assert!(engine.is_dq_immune("A"));
    }

    #[test]
    fn immunity_voids_a_dq_loss_but_not_a_pinfall() {
        let mut engine = engine_with(no_dq("SELF"), json!([]));
        engine.setup().unwrap();
        engine
            .apply_action(
                &Action::LoseBy {
                    kind: LoseKind::Disqualification,
                    who: Who::SelfSide,
                },
                "A",
                "",
            )
            .unwrap();
        assert!(
            engine.pending_loss.is_none(),
            "the DQ loss is voided, nothing is queued"
        );
        engine
            .apply_action(
                &Action::LoseBy {
                    kind: LoseKind::Pinfall,
                    who: Who::SelfSide,
                },
                "A",
                "",
            )
            .unwrap();
        // A pinfall is a different `kind`, so DQ immunity must not touch it. The loss
        // is QUEUED here (`pending_loss`) and settled at the next resolution point.
        assert_eq!(
            engine
                .pending_loss
                .as_ref()
                .map(|(l, r)| (l.as_str(), r.as_str())),
            Some(("A", "pinfall")),
        );
    }

    // -- last-played resolution (task #93) -----------------------------------

    /// A main-deck card carrying one Match-scoped `DisqualificationRule` toggle at
    /// `duration`, stamped with play-sequence `seq` (as if played on that tick).
    fn dq_card(uuid: &str, enabled: bool, duration: &str) -> Value {
        json!({
            "atk_type": "Grapple", "db_uuid": uuid, "name": uuid, "number": 2,
            "play_order": "Finish", "raw_text": "", "tags": [], "finish_bonuses": {},
            "effects": [{
                "@type": "Effect",
                "trigger": {"@type": "Static"},
                "condition": {"@type": "Always"},
                "actions": [{"@type": "DisqualificationRule", "enabled": enabled, "scope": "MATCH"}],
                "duration": duration,
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "dq toggle", "source": "card", "optional": false
            }]
        })
    }

    /// Push `card` into `key`'s `zone`, stamped with play-sequence `seq` (`played_seq`
    /// is `#[serde(skip)]`, so it is set here rather than deserialized).
    fn put(engine: &mut Engine, key: &str, zone: &str, card: Value, seq: u64) {
        let mut c: Card = serde_json::from_value(card).unwrap();
        c.played_seq = Some(seq);
        let p = engine.state.players.get_mut(key).unwrap();
        if zone == "discard" {
            p.discard.push(c);
        } else {
            p.in_play.push(c);
        }
    }

    #[test]
    fn no_toggle_leaves_dq_enabled() {
        let engine = engine_with(json!([]), json!([]));
        assert!(!engine.is_dq_immune("A"), "DQ is on by default");
    }

    #[test]
    fn a_discard_reenable_overrides_an_earlier_no_dq() {
        // A's gimmick declares no-DQ (sequence 0, present from setup). Later a "this
        // match has Disqualifications" card lands in A's discard (played tick 5). Last
        // played wins: DQ is back on, so A is no longer immune.
        let mut engine = engine_with(no_dq("MATCH"), json!([]));
        put(
            &mut engine,
            "A",
            "discard",
            dq_card("ref", true, "WHILE_IN_DISCARD"),
            5,
        );
        assert!(
            !engine.is_dq_immune("A"),
            "re-enable played after the no-DQ wins"
        );
    }

    #[test]
    fn a_later_no_dq_overrides_a_discard_reenable() {
        // The re-enable is already in discard (tick 3); then a no-DQ card is played
        // onto the board (tick 7). The most-recently-played toggle disables DQ again.
        let mut engine = engine_with(json!([]), json!([]));
        put(
            &mut engine,
            "A",
            "discard",
            dq_card("ref", true, "WHILE_IN_DISCARD"),
            3,
        );
        put(
            &mut engine,
            "A",
            "in_play",
            dq_card("nodq", false, "WHILE_IN_PLAY"),
            7,
        );
        assert!(engine.is_dq_immune("A"), "the later no-DQ wins");
    }

    #[test]
    fn a_reenable_in_play_is_inert() {
        // The same re-enable card sitting IN PLAY does nothing — its clause is
        // discard-scoped — so a standing no-DQ gimmick still grants immunity.
        let mut engine = engine_with(no_dq("MATCH"), json!([]));
        put(
            &mut engine,
            "A",
            "in_play",
            dq_card("ref", true, "WHILE_IN_DISCARD"),
            9,
        );
        assert!(
            engine.is_dq_immune("A"),
            "discard-scoped re-enable is inert in play"
        );
    }
}

/// `OnHit.who` (task #79 / El Super Hombre V2): an OnHit gimmick can key on the
/// OPPONENT's hit ("after your opponent hits a Follow Up"). Both players are scanned
/// at every hit; the default `SELF` must reproduce the pre-v43 behavior exactly.
#[cfg(test)]
mod on_hit_who_tests {
    use super::*;

    struct FirstLegal;

    impl Decider for FirstLegal {
        fn decide(
            &mut self,
            _point: &str,
            _viewer: &str,
            legal: &[Value],
            _state: &mut GameState,
        ) -> Option<Value> {
            legal.first().cloned()
        }

        fn policy_name(&self, _viewer: &str) -> String {
            "first-legal".to_owned()
        }
    }

    /// A "when `who` hits a Followup, draw 1" gimmick.
    fn on_hit_draw(who: &str) -> Value {
        json!([{
            "@type": "Effect",
            "trigger": {"@type": "OnHit", "atk_type": null, "name_contains": [],
                        "text_contains": [], "on_any": false, "order": "Followup",
                        "who": who},
            "condition": {"@type": "Always"},
            "actions": [{"@type": "Draw", "n": 1, "source": "TOP", "who": "SELF",
                         "per": null, "per_who": "SELF"}],
            "duration": "INSTANT",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "on hit draw 1", "source": "gimmick", "optional": false
        }])
    }

    fn engine_with(a_effects: Value) -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let cards: Vec<Value> = (0..10)
            .map(|i| {
                json!({"atk_type": "Strike", "db_uuid": format!("c{i}"), "effects": [],
                       "finish_bonuses": {}, "name": format!("c{i}"), "number": 1,
                       "play_order": "Lead", "raw_text": "", "tags": []})
            })
            .collect();
        let deck = |id: &str, effects: Value| -> Deck {
            serde_json::from_value(json!({
                "competitor": {"db_uuid": id, "name": id, "division": "World Championship",
                    "stats": stats, "effects": effects},
                "entrance": {"db_uuid": format!("{id}-ent"), "name": "ent"}, "cards": cards.clone(),
            }))
            .expect("deck")
        };
        Engine::new(
            deck("A", a_effects),
            deck("B", json!([])),
            Box::new(FirstLegal),
            1,
            String::new(),
            "sim".into(),
        )
    }

    fn followup() -> Card {
        serde_json::from_value(json!({
            "atk_type": "Strike", "db_uuid": "fu", "effects": [], "finish_bonuses": {},
            "name": "Follow Through", "number": 1, "play_order": "Followup",
            "raw_text": "", "tags": []
        }))
        .expect("card")
    }

    /// Cards A drew while `hitter` hit a Follow Up.
    fn a_drew_on_hit_by(engine: &mut Engine, hitter: &str) -> usize {
        let before = engine.state.players["A"].hand.len();
        engine.run_hit_gimmicks(&followup(), hitter).unwrap();
        engine.state.players["A"].hand.len() - before
    }

    #[test]
    fn who_opp_fires_only_on_the_opponents_hit() {
        let mut engine = engine_with(on_hit_draw("OPP"));
        engine.setup().unwrap();
        assert_eq!(
            a_drew_on_hit_by(&mut engine, "B"),
            1,
            "opponent's hit fires"
        );
        assert_eq!(a_drew_on_hit_by(&mut engine, "A"), 0, "own hit does not");
    }

    #[test]
    fn who_self_is_unchanged_from_before_the_field_existed() {
        // The default. Every pre-v43 OnHit node carries SELF, so this is the
        // regression guard for the whole existing corpus.
        let mut engine = engine_with(on_hit_draw("SELF"));
        engine.setup().unwrap();
        assert_eq!(a_drew_on_hit_by(&mut engine, "A"), 1, "own hit fires");
        assert_eq!(
            a_drew_on_hit_by(&mut engine, "B"),
            0,
            "opponent's hit does not"
        );
    }

    #[test]
    fn the_play_order_gate_still_applies_to_an_opponent_scoped_hit() {
        // who=OPP composes with the existing order gate rather than bypassing it.
        let mut engine = engine_with(on_hit_draw("OPP"));
        engine.setup().unwrap();
        let lead: Card = serde_json::from_value(json!({
            "atk_type": "Strike", "db_uuid": "ld", "effects": [], "finish_bonuses": {},
            "name": "Jab", "number": 1, "play_order": "Lead", "raw_text": "", "tags": []
        }))
        .expect("card");
        let before = engine.state.players["A"].hand.len();
        engine.run_hit_gimmicks(&lead, "B").unwrap();
        assert_eq!(
            engine.state.players["A"].hand.len(),
            before,
            "a Lead does not satisfy an order=Followup gate"
        );
    }
}

#[cfg(test)]
mod min_hand_size_tests {
    use super::*;
    use serde_json::{json, Value};

    /// A Static hand modifier: `kind` is "MaxHandSize" or "MinHandSize".
    fn hand_mod(kind: &str, delta: i64, who: &str) -> Value {
        json!({
            "@type": "Effect",
            "trigger": {"@type": "Static"},
            "condition": {"@type": "Always"},
            "actions": [{"@type": kind, "delta": delta, "who": who, "duration": "WHILE_IN_PLAY"}],
            "duration": "WHILE_IN_PLAY",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "hand mod", "source": "gimmick", "optional": false
        })
    }

    /// Engine where A's gimmick carries `a_effects` and B's carries `b_effects`.
    fn engine_with(a_effects: Value, b_effects: Value) -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let deck = |id: &str, effects: Value| -> Deck {
            serde_json::from_value(json!({
                "competitor": {"db_uuid": id, "name": id, "division": "World Championship",
                    "stats": stats, "effects": effects},
                "entrance": {"db_uuid": format!("{id}-ent"), "name": "ent"}, "cards": [],
            }))
            .expect("deck")
        };
        Engine::new(
            deck("A", a_effects),
            deck("B", b_effects),
            Box::new(FirstLegal),
            1,
            String::new(),
            "sim".into(),
        )
    }

    /// Never invoked (these tests only read the derived cap), but `Engine::new`
    /// requires a decider.
    struct FirstLegal;

    impl Decider for FirstLegal {
        fn decide(
            &mut self,
            _point: &str,
            _viewer: &str,
            legal: &[Value],
            _state: &mut GameState,
        ) -> Option<Value> {
            legal.first().cloned()
        }

        fn policy_name(&self, _viewer: &str) -> String {
            "first-legal".to_owned()
        }
    }

    fn cap(engine: &Engine, key: &str, base: i64) -> i64 {
        engine.state.effective_hand_cap(key, base, None)
    }

    #[test]
    fn default_floor_is_the_minimum_not_zero() {
        // A max reduction of -20 would give -10; the default minimum (3) floors it.
        let engine = engine_with(json!([]), json!([hand_mod("MaxHandSize", -20, "OPP")]));
        assert_eq!(cap(&engine, "A", 10), 3);
    }

    #[test]
    fn min_handsize_raises_the_floor_on_a_reduced_cap() {
        // A's min +2 (floor 5); B reduces A's max to 4 -> clamped up to 5.
        let engine = engine_with(
            json!([hand_mod("MinHandSize", 2, "SELF")]),
            json!([hand_mod("MaxHandSize", -6, "OPP")]),
        );
        assert_eq!(cap(&engine, "A", 10), 5);
    }

    #[test]
    fn min_handsize_alone_does_not_lower_a_healthy_cap() {
        let engine = engine_with(json!([hand_mod("MinHandSize", 2, "SELF")]), json!([]));
        assert_eq!(cap(&engine, "A", 10), 10);
    }

    #[test]
    fn quadruple_h_min_and_max_plus_two() {
        // max 12, min floor 5 -> cap = max(12, 5) = 12.
        let engine = engine_with(
            json!([
                hand_mod("MaxHandSize", 2, "SELF"),
                hand_mod("MinHandSize", 2, "SELF")
            ]),
            json!([]),
        );
        assert_eq!(cap(&engine, "A", 10), 12);
    }

    #[test]
    fn min_above_max_becomes_new_max() {
        // max -6 (=4), min +4 (floor 7) -> cap 7.
        let engine = engine_with(
            json!([hand_mod("MinHandSize", 4, "SELF")]),
            json!([hand_mod("MaxHandSize", -6, "OPP")]),
        );
        assert_eq!(cap(&engine, "A", 10), 7);
    }
}

#[cfg(test)]
mod jokerfish_stop_tests {
    use super::*;
    use serde_json::{json, Value};

    /// A Static effect whose sole action is `node` (a stop declaration).
    fn decl(node: Value) -> Value {
        json!({
            "@type": "Effect",
            "trigger": {"@type": "Static"},
            "condition": {"@type": "Always"},
            "actions": [node],
            "duration": "WHILE_IN_PLAY",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "decl", "source": "gimmick", "optional": false
        })
    }

    /// A card of deck `number` carrying a `Stop{order}` (any atk_type).
    fn stopper(number: i64, order: &str) -> Card {
        serde_json::from_value(json!({
            "atk_type": "Strike", "db_uuid": "stop", "name": "stop", "number": number,
            "play_order": "Lead", "raw_text": "", "tags": [], "finish_bonuses": {},
            "effects": [{
                "@type": "Effect", "trigger": {"@type": "Static"}, "condition": {"@type": "Always"},
                "actions": [{"@type": "Stop", "order": order, "atk_type": null,
                             "source_is_skillreq": false}],
                "duration": "INSTANT",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "stop", "source": "card", "optional": false
            }]
        }))
        .expect("stopper")
    }

    fn attack(order: &str) -> Card {
        serde_json::from_value(json!({
            "atk_type": "Strike", "db_uuid": "atk", "name": "atk", "number": 1,
            "play_order": order, "raw_text": "", "tags": [], "finish_bonuses": {}, "effects": []
        }))
        .expect("attack")
    }

    /// Engine where DEFENDER A's gimmick carries `a_effects`.
    fn engine_with(a_effects: Value) -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let deck = |id: &str, effects: Value| -> Deck {
            serde_json::from_value(json!({
                "competitor": {"db_uuid": id, "name": id, "division": "World Championship",
                    "stats": stats, "effects": effects},
                "entrance": {"db_uuid": format!("{id}-ent"), "name": "ent"}, "cards": [],
            }))
            .expect("deck")
        };
        Engine::new(
            deck("A", a_effects),
            deck("B", json!([])),
            Box::new(NoDecider),
            1,
            String::new(),
            "sim".into(),
        )
    }

    struct NoDecider;
    impl Decider for NoDecider {
        fn decide(
            &mut self,
            _: &str,
            _: &str,
            legal: &[Value],
            _: &mut GameState,
        ) -> Option<Value> {
            legal.first().cloned()
        }
        fn policy_name(&self, _: &str) -> String {
            "none".to_owned()
        }
    }

    #[test]
    fn followup_stop_cannot_stop_a_finish_without_the_reframe() {
        let engine = engine_with(json!([]));
        assert!(!engine.card_can_stop("A", &stopper(1, "Followup"), &attack("Finish")));
    }

    #[test]
    fn reframe_lets_a_followup_stop_catch_the_opponents_finish() {
        // Jokerfish: "your opponent's Finishes are also Follow Ups for your Stop cards".
        let engine = engine_with(json!([decl(
            json!({"@type": "StopCountsOrderAs", "attack_order": "Finish", "as_order": "Followup"})
        )]));
        assert!(engine.card_can_stop("A", &stopper(1, "Followup"), &attack("Finish")));
    }

    #[test]
    fn reframe_does_not_touch_an_unrelated_order() {
        // The reframe only maps Finish->Follow Up; a Lead attack is still unstoppable
        // by a Follow-Up stop.
        let engine = engine_with(json!([decl(
            json!({"@type": "StopCountsOrderAs", "attack_order": "Finish", "as_order": "Followup"})
        )]));
        assert!(!engine.card_can_stop("A", &stopper(1, "Followup"), &attack("Lead")));
    }

    #[test]
    fn suppress_stop_disables_a_card_in_the_number_range() {
        // "your cards #19-21 cannot stop cards": a #20 stop is disabled, a #18 is not.
        let engine = engine_with(json!([decl(
            json!({"@type": "SuppressStop", "number_min": 19, "number_max": 21})
        )]));
        assert!(!engine.card_can_stop("A", &stopper(20, "Lead"), &attack("Lead")));
        assert!(engine.card_can_stop("A", &stopper(18, "Lead"), &attack("Lead")));
    }

    #[test]
    fn suppress_stop_is_inert_without_the_declaration() {
        let engine = engine_with(json!([]));
        assert!(engine.card_can_stop("A", &stopper(20, "Lead"), &attack("Lead")));
    }
}

#[cfg(test)]
mod even_unstoppable_stop_tests {
    use super::*;
    use serde_json::{json, Value};

    /// A stop card carrying a `Stop{order, even_unstoppable}` (any atk_type).
    fn stopper(order: &str, even_unstoppable: bool) -> Card {
        serde_json::from_value(json!({
            "atk_type": "Strike", "db_uuid": "stop", "name": "stop", "number": 1,
            "play_order": "Lead", "raw_text": "", "tags": [], "finish_bonuses": {},
            "effects": [{
                "@type": "Effect", "trigger": {"@type": "Static"}, "condition": {"@type": "Always"},
                "actions": [{"@type": "Stop", "order": order, "atk_type": null,
                             "source_is_skillreq": false, "even_unstoppable": even_unstoppable}],
                "duration": "INSTANT",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "stop", "source": "card", "optional": false
            }]
        }))
        .expect("stopper")
    }

    /// A Finish attack, optionally declaring itself `Unstoppable` by anything.
    fn attack(unstoppable: bool) -> Card {
        let effects: Value = if unstoppable {
            json!([{
                "@type": "Effect", "trigger": {"@type": "Static"}, "condition": {"@type": "Always"},
                "actions": [{"@type": "Unstoppable", "by_order": null}],
                "duration": "WHILE_IN_PLAY",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "u", "source": "card", "optional": false
            }])
        } else {
            json!([])
        };
        serde_json::from_value(json!({
            "atk_type": "Strike", "db_uuid": "atk", "name": "atk", "number": 1,
            "play_order": "Finish", "raw_text": "", "tags": [], "finish_bonuses": {},
            "effects": effects
        }))
        .expect("attack")
    }

    fn engine() -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let deck = |id: &str| -> Deck {
            serde_json::from_value(json!({
                "competitor": {"db_uuid": id, "name": id, "division": "World Championship",
                    "stats": stats, "effects": []},
                "entrance": {"db_uuid": format!("{id}-ent"), "name": "ent"}, "cards": [],
            }))
            .expect("deck")
        };
        Engine::new(
            deck("A"),
            deck("B"),
            Box::new(NoDecider),
            1,
            String::new(),
            "sim".into(),
        )
    }

    struct NoDecider;
    impl Decider for NoDecider {
        fn decide(&mut self, _: &str, _: &str, l: &[Value], _: &mut GameState) -> Option<Value> {
            l.first().cloned()
        }
        fn policy_name(&self, _: &str) -> String {
            "none".to_owned()
        }
    }

    #[test]
    fn plain_stop_cannot_catch_an_unstoppable_attack() {
        let engine = engine();
        assert!(!engine.card_can_stop("A", &stopper("Finish", false), &attack(true)));
    }

    #[test]
    fn even_unstoppable_stop_catches_the_unstoppable_attack() {
        // "Stop any Finish Strike that cannot be stopped."
        let engine = engine();
        assert!(engine.card_can_stop("A", &stopper("Finish", true), &attack(true)));
    }

    #[test]
    fn even_unstoppable_stop_still_catches_a_normal_attack() {
        let engine = engine();
        assert!(engine.card_can_stop("A", &stopper("Finish", true), &attack(false)));
    }

    /// A Finish attack declaring "If the Crowd Meter is 5 or greater, this card
    /// cannot be stopped" — a condition-gated `Unstoppable`.
    fn crowd_gated_attack() -> Card {
        serde_json::from_value(json!({
            "atk_type": "Strike", "db_uuid": "atk", "name": "atk", "number": 1,
            "play_order": "Finish", "raw_text": "", "tags": [], "finish_bonuses": {},
            "effects": [{
                "@type": "Effect", "trigger": {"@type": "Static"},
                "condition": {"@type": "CrowdMeterCompare", "cmp": ">=", "value": 5},
                "actions": [{"@type": "Unstoppable", "by_order": null}],
                "duration": "WHILE_IN_PLAY",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "u", "source": "card", "optional": false
            }]
        }))
        .expect("attack")
    }

    /// A stop card of the given `name` carrying a plain `Stop{Finish}`.
    fn named_stopper(name: &str) -> Card {
        serde_json::from_value(json!({
            "atk_type": "Strike", "db_uuid": "s", "name": name, "number": 1,
            "play_order": "Lead", "raw_text": "", "tags": [], "finish_bonuses": {},
            "effects": [{
                "@type": "Effect", "trigger": {"@type": "Static"}, "condition": {"@type": "Always"},
                "actions": [{"@type": "Stop", "order": "Finish", "atk_type": null,
                             "source_is_skillreq": false, "even_unstoppable": false}],
                "duration": "INSTANT",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "stop", "source": "card", "optional": false
            }]
        }))
        .expect("named stopper")
    }

    /// A Finish attack declaring "Cannot be stopped by \"Beg for Mercy\"".
    fn name_gated_attack() -> Card {
        serde_json::from_value(json!({
            "atk_type": "Strike", "db_uuid": "atk", "name": "atk", "number": 1,
            "play_order": "Finish", "raw_text": "", "tags": [], "finish_bonuses": {},
            "effects": [{
                "@type": "Effect", "trigger": {"@type": "Static"}, "condition": {"@type": "Always"},
                "actions": [{"@type": "Unstoppable", "by_order": null, "by_name": "Beg for Mercy"}],
                "duration": "WHILE_IN_PLAY",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "u", "source": "card", "optional": false
            }]
        }))
        .expect("attack")
    }

    #[test]
    fn name_gated_unstoppable_only_blocks_the_named_stopper() {
        let engine = engine();
        // The named stopper "Beg for Mercy" cannot stop it…
        assert!(!engine.card_can_stop("A", &named_stopper("Beg for Mercy"), &name_gated_attack()));
        // …but any other stopper still can.
        assert!(engine.card_can_stop("A", &named_stopper("School Boy"), &name_gated_attack()));
    }

    #[test]
    fn conditional_unstoppable_honors_its_condition() {
        let mut engine = engine();
        // Condition false (Crowd Meter 0) -> the card is stoppable by a plain stop.
        engine.state.crowd_meter = 0;
        assert!(engine.card_can_stop("A", &stopper("Finish", false), &crowd_gated_attack()));
        // Condition true (Crowd Meter 5) -> unstoppable; the plain stop can't catch it.
        engine.state.crowd_meter = 5;
        assert!(!engine.card_can_stop("A", &stopper("Finish", false), &crowd_gated_attack()));
        // …but an even_unstoppable stop still catches it even when unstoppable.
        assert!(engine.card_can_stop("A", &stopper("Finish", true), &crowd_gated_attack()));
    }

    /// A Finish attack declaring "When your opponent's turn roll is 5 this card cannot
    /// be stopped" — an opp-turn-roll-VALUE-gated `Unstoppable` (Scott's Loaded Glove).
    fn opp_roll_gated_attack() -> Card {
        serde_json::from_value(json!({
            "atk_type": "Strike", "db_uuid": "atk", "name": "atk", "number": 1,
            "play_order": "Finish", "raw_text": "", "tags": [], "finish_bonuses": {},
            "effects": [{
                "@type": "Effect", "trigger": {"@type": "Static"},
                "condition": {"@type": "RollValue", "cmp": "=", "value": 5, "who": "OPP"},
                "actions": [{"@type": "Unstoppable", "by_order": null}],
                "duration": "WHILE_IN_PLAY",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "u", "source": "card", "optional": false
            }]
        }))
        .expect("attack")
    }

    #[test]
    fn opp_turn_roll_value_gates_unstoppable() {
        use crate::conditions::RollContext;
        // Defender A tries to stop B's finish → the ATTACKER is B, whose "opponent's turn
        // roll" is the defender A's value, derived from B's context as `value + gap`
        // (gap = opp − self). B rolled 3, gap 2 → A (the opponent) rolled 5, so the gate
        // holds and A's plain stop can't catch the finish.
        let mut engine = engine();
        engine.roll_ctx.insert(
            "B".into(),
            RollContext {
                skill: Some(Skill::Strike),
                gap: Some(2),
                value: Some(3),
                opp_skill: Some(Skill::Strike),
            },
        );
        assert!(!engine.card_can_stop("A", &stopper("Finish", false), &opp_roll_gated_attack()));

        // A rolled 4 (B value 3, gap 1) → gate false → the plain stop catches it.
        engine.roll_ctx.insert(
            "B".into(),
            RollContext {
                skill: Some(Skill::Strike),
                gap: Some(1),
                value: Some(3),
                opp_skill: Some(Skill::Strike),
            },
        );
        assert!(engine.card_can_stop("A", &stopper("Finish", false), &opp_roll_gated_attack()));
    }

    /// A stop card carrying the synthetic SkillRequirement tag.
    fn skillreq_stopper() -> Card {
        serde_json::from_value(json!({
            "atk_type": "Strike", "db_uuid": "sr", "name": "sr", "number": 1,
            "play_order": "Lead", "raw_text": "", "tags": ["SkillRequirement"], "finish_bonuses": {},
            "effects": [{
                "@type": "Effect", "trigger": {"@type": "Static"}, "condition": {"@type": "Always"},
                "actions": [{"@type": "Stop", "order": "Finish", "atk_type": null,
                             "source_is_skillreq": false, "even_unstoppable": false}],
                "duration": "INSTANT",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "stop", "source": "card", "optional": false
            }]
        }))
        .expect("sr stopper")
    }

    /// An Unstoppable{by_skillreq} effect (as an authored gimmick/card clause).
    fn skillreq_unstoppable(source: &str) -> Value {
        json!({
            "@type": "Effect", "trigger": {"@type": "Static"}, "condition": {"@type": "Always"},
            "actions": [{"@type": "Unstoppable", "by_order": null, "by_name": null, "by_skillreq": true}],
            "duration": "WHILE_IN_PLAY",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "u", "source": source, "optional": false
        })
    }

    /// A Finish attack that itself declares "Cannot be stopped by Skill Requirement cards".
    fn skillreq_attack() -> Card {
        serde_json::from_value(json!({
            "atk_type": "Strike", "db_uuid": "atk", "name": "atk", "number": 1,
            "play_order": "Finish", "raw_text": "", "tags": [], "finish_bonuses": {},
            "effects": [skillreq_unstoppable("card")]
        }))
        .expect("sr attack")
    }

    /// A stop card whose `Stop` targets `atk_type` AND carries a name/text filter.
    fn target_stopper(named: bool, needle: &str) -> Card {
        let target = if named {
            json!({"name_contains": [needle], "text_contains": []})
        } else {
            json!({"name_contains": [], "text_contains": [needle]})
        };
        serde_json::from_value(json!({
            "atk_type": "Strike", "db_uuid": "t", "name": "t", "number": 1,
            "play_order": "Lead", "raw_text": "", "tags": [], "finish_bonuses": {},
            "effects": [{
                "@type": "Effect", "trigger": {"@type": "Static"}, "condition": {"@type": "Always"},
                "actions": [{"@type": "Stop", "order": null, "atk_type": "Submission",
                             "source_is_skillreq": false, "even_unstoppable": false, "target": target}],
                "duration": "INSTANT",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "stop", "source": "card", "optional": false
            }]
        }))
        .expect("target stopper")
    }

    /// A Submission attack with the given name and raw_text.
    fn named_attack(name: &str, text: &str) -> Card {
        serde_json::from_value(json!({
            "atk_type": "Submission", "db_uuid": "a", "name": name, "number": 3,
            "play_order": "Lead", "raw_text": text, "tags": [], "finish_bonuses": {}, "effects": []
        }))
        .expect("attack")
    }

    #[test]
    fn target_filtered_stop_only_catches_a_matching_attack() {
        let engine = engine();
        // "Stop any Submission with \"Rope\" in the name."
        let by_name = target_stopper(true, "Rope");
        assert!(engine.card_can_stop("A", &by_name, &named_attack("Rope-a-Dope", "")));
        assert!(!engine.card_can_stop("A", &by_name, &named_attack("Clothesline", "")));
        // "Stop any Submission with \"Disqualification\" in the text."
        let by_text = target_stopper(false, "Disqualification");
        assert!(engine.card_can_stop(
            "A",
            &by_text,
            &named_attack("X", "Win via Disqualification.")
        ));
        assert!(!engine.card_can_stop("A", &by_text, &named_attack("X", "Draw 1 card.")));
    }

    #[test]
    fn skillreq_unstoppable_blocks_only_skill_requirement_stoppers() {
        let engine = engine();
        // A skill-requirement stopper cannot stop it…
        assert!(!engine.card_can_stop("A", &skillreq_stopper(), &skillreq_attack()));
        // …but a stopper without the requirement still can.
        assert!(engine.card_can_stop("A", &named_stopper("School Boy"), &skillreq_attack()));
    }

    #[test]
    fn player_scope_skillreq_from_gimmick_shields_every_attack() {
        let mut engine = engine();
        // B (the attacker, opposite defender A) declares "Your cards cannot be stopped
        // by Skill Requirement cards" on their gimmick — a player-scope declaration.
        engine
            .state
            .players
            .get_mut("B")
            .unwrap()
            .competitor
            .effects
            .push(serde_json::from_value(skillreq_unstoppable("gimmick")).unwrap());
        // A PLAIN attack (no own Unstoppable) is now unstoppable vs a skill-req stopper…
        assert!(!engine.card_can_stop("A", &skillreq_stopper(), &attack(false)));
        // …but an ordinary stopper still stops it (the shield is only vs skill-req).
        assert!(engine.card_can_stop("A", &stopper("Finish", false), &attack(false)));
    }
}

#[cfg(test)]
mod man_from_it_tests {
    use super::*;
    use crate::policy::{HeuristicPolicy, Policies};
    use serde_json::{json, Value};

    fn card(uuid: &str, order: &str, number: i64) -> Value {
        json!({"atk_type": "Strike", "db_uuid": uuid, "name": uuid, "number": number,
               "play_order": order, "raw_text": "", "tags": [], "finish_bonuses": {}, "effects": []})
    }

    fn heuristic_pair() -> Policies {
        Policies::new(
            Box::new(HeuristicPolicy::heuristic()),
            Box::new(HeuristicPolicy::heuristic()),
        )
    }

    fn engine_with(a_effects: Value) -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let deck = |id: &str, effects: Value| -> Deck {
            serde_json::from_value(json!({
                "competitor": {"db_uuid": id, "name": id, "division": "World Championship",
                    "stats": stats, "effects": effects},
                "entrance": {"db_uuid": format!("{id}-ent"), "name": "ent"}, "cards": [],
            }))
            .expect("deck")
        };
        Engine::new(
            deck("A", a_effects),
            deck("B", json!([])),
            Box::new(heuristic_pair()),
            1,
            String::new(),
            "sim".into(),
        )
    }

    /// An OnFinishRoll(Technique) gimmick that draws 1 when it fires.
    fn on_finish_draw() -> Value {
        json!([{
            "@type": "Effect",
            "trigger": {"@type": "OnFinishRoll", "skill": "Technique", "who": "SELF"},
            "condition": {"@type": "Always"},
            "actions": [{"@type": "Draw", "n": 1, "source": "TOP", "who": "SELF",
                         "cap": null, "per": null, "per_excludes_trigger": false, "per_who": "SELF"}],
            "duration": "INSTANT",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "d", "source": "gimmick", "optional": false
        }])
    }

    #[test]
    fn on_finish_roll_fires_only_on_the_gated_skill() {
        let mut engine = engine_with(on_finish_draw());
        engine.state.players.get_mut("A").unwrap().deck =
            vec![serde_json::from_value(card("c1", "Lead", 1)).unwrap()];
        // A Power finish roll does not fire the Technique gimmick.
        engine.run_on_finish_roll("A", Skill::Power, 20).unwrap();
        assert_eq!(engine.state.players["A"].hand.len(), 0);
        // A Technique finish roll fires it: A draws 1.
        engine
            .run_on_finish_roll("A", Skill::Technique, 20)
            .unwrap();
        assert_eq!(engine.state.players["A"].hand.len(), 1);
    }

    #[test]
    fn choose_hand_bury_lets_the_attacker_bury_the_opponents_best() {
        // A buries 1 of B's Follow Up / Finish hand cards, choosing (sabotage = the
        // most valuable, a Finish). B's Lead is out of the filter and untouched.
        let mut engine = engine_with(json!([]));
        engine.state.players.get_mut("B").unwrap().hand = vec![
            serde_json::from_value(card("b-lead", "Lead", 1)).unwrap(),
            serde_json::from_value(card("b-fu", "Followup", 2)).unwrap(),
            serde_json::from_value(card("b-fin", "Finish", 3)).unwrap(),
        ];
        let selector: CardFilter = serde_json::from_value(json!({
            "@type": "CardFilter", "number": null, "atk_type": null, "play_order": null,
            "play_orders": ["Followup", "Finish"], "tag": null, "name": null, "raw": null,
            "name_contains": [], "text_contains": []
        }))
        .unwrap();
        let spec = BurySpec {
            selector,
            count: 1,
            who: Who::Opp,
            random: false,
            source: BuryFrom::Hand,
            choose: true,
        };
        engine.act_bury(spec, "A").unwrap();
        let b = &engine.state.players["B"];
        // The Finish was buried (moved to B's deck bottom); the Lead stayed in hand.
        assert!(b.hand.iter().any(|c| c.db_uuid == "b-lead"));
        assert!(b.hand.iter().any(|c| c.db_uuid == "b-fu"));
        assert!(!b.hand.iter().any(|c| c.db_uuid == "b-fin"));
        assert!(b.deck.iter().any(|c| c.db_uuid == "b-fin"));
    }

    /// Defector's Dismantler (schema v76): `CoupledDiscard{offset:-1}` — the actor
    /// sheds N = min(self_hand, opp_hand+1), the opponent sheds max(0, N-1). With
    /// A=5, B=3: N=4 → A keeps 1; opp sheds 3 → B empties.
    #[test]
    fn coupled_discard_strips_the_opponent_hand() {
        let mut engine = engine_with(json!([]));
        let mk = |u: &str| -> Card { serde_json::from_value(card(u, "Lead", 1)).unwrap() };
        engine.state.players.get_mut("A").unwrap().hand =
            (0..5).map(|i| mk(&format!("a{i}"))).collect();
        engine.state.players.get_mut("B").unwrap().hand =
            (0..3).map(|i| mk(&format!("b{i}"))).collect();
        engine.act_coupled_discard(-1, "A").unwrap();
        assert_eq!(engine.state.players["A"].hand.len(), 1, "A sheds 4 of 5");
        assert_eq!(engine.state.players["B"].hand.len(), 0, "B sheds 3 (N-1)");
    }

    /// A finish-roll re-roll (`Reroll{when:This, finish:true}`, schema v76) is offered
    /// only when its gate matches the finish skill. Gated on Agility here: offered on
    /// an Agility finish roll, not on a Power one.
    #[test]
    fn finish_reroll_offered_only_on_the_gated_skill() {
        let reroll = json!([{
            "@type": "Effect", "trigger": {"@type": "Static"},
            "condition": {"@type": "RollWasSkill", "skill": "Agility", "who": "SELF"},
            "actions": [{"@type": "Reroll", "who": "SELF", "once": true, "choose": false,
                "when": "THIS", "cost": null, "finish": true}],
            "duration": "WHILE_IN_PLAY", "optional": false,
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "", "source": "gimmick"
        }]);
        let mut engine = engine_with(reroll);
        assert!(
            engine.offer_finish_reroll("A", Skill::Agility).unwrap(),
            "rolled Agility → re-roll offered"
        );
        assert!(
            !engine.offer_finish_reroll("A", Skill::Power).unwrap(),
            "rolled Power → gate fails, no re-roll"
        );
    }

    /// A `Duration::WhileInDiscard` triggered effect fires from the discard pile via
    /// `triggered_effects` at trigger-dispatch sites — Ricky Riot's Soups Up Stunner:
    /// in discard + opponent rolled ≥3 higher (`RollGapAtLeast{3}`) grants a next-turn
    /// re-roll (`Reroll{Next}`). It stays dormant until the card reaches the discard.
    #[test]
    fn while_in_discard_onroll_reroll_fires_from_the_discard() {
        let soups: Card = serde_json::from_value(json!({
            "atk_type":"Grapple","db_uuid":"soups","name":"Soups Up","number":29,
            "play_order":"Finish","raw_text":"","tags":[],"finish_bonuses":{},
            "effects":[{"@type":"Effect","trigger":{"@type":"OnRoll","skill":null,"who":"SELF"},
                "condition":{"@type":"RollGapAtLeast","k":3},
                "actions":[{"@type":"Reroll","who":"SELF","once":true,"choose":false,
                    "when":"NEXT","cost":null,"finish":false}],
                "duration":"WHILE_IN_DISCARD","optional":false,
                "frequency":{"@type":"FrequencyGuard","kind":"UNLIMITED","n":null},
                "raw_clause":"","source":"card"}]
        }))
        .unwrap();
        let ctx = |gap| RollContext {
            skill: Some(Skill::Strike),
            gap: Some(gap),
            value: None,
            opp_skill: None,
        };

        // In hand (not discard): the WHILE_IN_DISCARD effect is dormant even at gap 3.
        let mut engine = engine_with(json!([]));
        engine.state.players.get_mut("A").unwrap().hand = vec![soups.clone()];
        engine.roll_ctx.insert("A".into(), ctx(3));
        engine.run_on_roll("A").unwrap();
        assert_eq!(
            engine.state.players["A"].reroll_grants.next_turn, 0,
            "dormant while in hand"
        );

        // In discard, opponent rolled only 2 higher: the gate fails, no grant.
        let mut engine = engine_with(json!([]));
        engine.state.players.get_mut("A").unwrap().discard = vec![soups.clone()];
        engine.roll_ctx.insert("A".into(), ctx(2));
        engine.run_on_roll("A").unwrap();
        assert_eq!(
            engine.state.players["A"].reroll_grants.next_turn, 0,
            "gap 2 < 3, no grant"
        );

        // In discard, opponent rolled 3 higher: the discard effect fires, grants a re-roll.
        let mut engine = engine_with(json!([]));
        engine.state.players.get_mut("A").unwrap().discard = vec![soups];
        engine.roll_ctx.insert("A".into(), ctx(3));
        engine.run_on_roll("A").unwrap();
        assert_eq!(
            engine.state.players["A"].reroll_grants.next_turn, 1,
            "discard OnRoll granted a next-turn re-roll"
        );
    }

    /// A `WhileInDiscard` OnRoll `AddSelfToHand` (task #115): "When this card is in your
    /// discard pile and you roll <S> for your turn roll, add it to your hand." The card
    /// resurrects ITSELF from the discard — `run_on_roll` binds the source card as
    /// `self_card` so `AddSelfToHand` moves the right one (discard → hand).
    #[test]
    fn while_in_discard_onroll_adds_itself_to_hand() {
        let card: Card = serde_json::from_value(json!({
            "atk_type":"Strike","db_uuid":"recur","name":"Comeback","number":30,
            "play_order":"Lead","raw_text":"","tags":[],"finish_bonuses":{},
            "effects":[{"@type":"Effect","trigger":{"@type":"OnRoll","skill":"Power","who":"SELF"},
                "condition":{"@type":"Always"},
                "actions":[{"@type":"AddSelfToHand"}],
                "duration":"WHILE_IN_DISCARD","optional":false,
                "frequency":{"@type":"FrequencyGuard","kind":"UNLIMITED","n":null},
                "raw_clause":"","source":"card"}]
        }))
        .unwrap();
        let ctx = |s| RollContext {
            skill: Some(s),
            gap: None,
            value: None,
            opp_skill: None,
        };

        // Rolled Technique (not Power): the skill gate fails, card stays in discard.
        let mut engine = engine_with(json!([]));
        engine.state.players.get_mut("A").unwrap().discard = vec![card.clone()];
        engine.roll_ctx.insert("A".into(), ctx(Skill::Technique));
        engine.run_on_roll("A").unwrap();
        assert_eq!(
            engine.state.players["A"].discard.len(),
            1,
            "wrong skill: stays"
        );
        assert_eq!(engine.state.players["A"].hand.len(), 0);

        // Rolled Power: the discard OnRoll fires and the card moves ITSELF to the hand.
        let mut engine = engine_with(json!([]));
        engine.state.players.get_mut("A").unwrap().discard = vec![card];
        engine.roll_ctx.insert("A".into(), ctx(Skill::Power));
        engine.run_on_roll("A").unwrap();
        assert_eq!(
            engine.state.players["A"].discard.len(),
            0,
            "left the discard"
        );
        assert_eq!(
            engine.state.players["A"]
                .hand
                .iter()
                .filter(|c| c.db_uuid == "recur")
                .count(),
            1,
            "resurrected itself into the hand"
        );
    }

    /// A `WhileInDiscard` OnHit self-resurrect (task #115 slice 2): "When this card is in
    /// your discard pile and you hit a card with 'Suplex' in the name, shuffle it into
    /// your deck." `run_hit_gimmicks` binds the discard-pile source as `self_card` so
    /// `ShuffleSelfIntoDeck` moves the RIGHT card (discard → deck), and the name gate on
    /// the HIT card decides whether it fires at all.
    #[test]
    fn while_in_discard_onhit_shuffles_itself_into_deck() {
        let watcher: Card = serde_json::from_value(json!({
            "atk_type":"Grapple","db_uuid":"recur","name":"Comeback","number":30,
            "play_order":"Lead","raw_text":"","tags":[],"finish_bonuses":{},
            "effects":[{"@type":"Effect",
                "trigger":{"@type":"OnHit","atk_type":null,"name_contains":["Suplex"],
                    "text_contains":[],"on_any":false,"order":null,"who":"SELF"},
                "condition":{"@type":"Always"},
                "actions":[{"@type":"ShuffleSelfIntoDeck"}],
                "duration":"WHILE_IN_DISCARD","optional":false,
                "frequency":{"@type":"FrequencyGuard","kind":"UNLIMITED","n":null},
                "raw_clause":"","source":"card"}]
        }))
        .unwrap();
        let hit = |name: &str| -> Card {
            serde_json::from_value(json!({
                "atk_type":"Strike","db_uuid":"hit","name":name,"number":1,
                "play_order":"Lead","raw_text":"","tags":[],"finish_bonuses":{},"effects":[]
            }))
            .unwrap()
        };

        // Hit a card WITHOUT "Suplex": the name gate fails, the watcher stays in discard.
        let mut engine = engine_with(json!([]));
        engine.state.players.get_mut("A").unwrap().discard = vec![watcher.clone()];
        engine.run_hit_gimmicks(&hit("Dropkick"), "A").unwrap();
        assert_eq!(
            engine.state.players["A"].discard.len(),
            1,
            "no name match: stays"
        );
        assert_eq!(engine.state.players["A"].deck.len(), 0);

        // Hit a "German Suplex": the discard OnHit fires and the watcher shuffles ITSELF
        // from the discard into the deck.
        let mut engine = engine_with(json!([]));
        engine.state.players.get_mut("A").unwrap().discard = vec![watcher];
        engine.run_hit_gimmicks(&hit("German Suplex"), "A").unwrap();
        assert_eq!(
            engine.state.players["A"].discard.len(),
            0,
            "left the discard"
        );
        assert_eq!(
            engine.state.players["A"]
                .deck
                .iter()
                .filter(|c| c.db_uuid == "recur")
                .count(),
            1,
            "shuffled itself into the deck"
        );
    }

    /// A `WhileInDiscard` OnStop self-resurrect (task #115 slice 2b): "When this card is
    /// in your discard pile and your opponent stops your card, add it to your hand." The
    /// clause carries `dir=Yours` (your card was stopped), so `run_on_stop_gimmicks` only
    /// resurrects it on the matching-direction call — not on the stopper's `Theirs` call.
    #[test]
    fn while_in_discard_onstop_dir_gated_self_resurrect() {
        let watcher = |db: &str| -> Card {
            serde_json::from_value(json!({
                "atk_type":"Grapple","db_uuid":db,"name":"Comeback","number":30,
                "play_order":"Lead","raw_text":"","tags":[],"finish_bonuses":{},
                "effects":[{"@type":"Effect",
                    "trigger":{"@type":"OnStop","dir":"YOURS","order":null},
                    "condition":{"@type":"Always"},
                    "actions":[{"@type":"AddSelfToHand"}],
                    "duration":"WHILE_IN_DISCARD","optional":false,
                    "frequency":{"@type":"FrequencyGuard","kind":"UNLIMITED","n":null},
                    "raw_clause":"","source":"card"}]
            }))
            .unwrap()
        };

        // The `Theirs` call (the stopper's vantage) must NOT fire a `dir=Yours` watcher.
        let mut engine = engine_with(json!([]));
        engine.state.players.get_mut("A").unwrap().discard = vec![watcher("w")];
        engine
            .run_on_stop_gimmicks("A", Direction::Theirs, PlayOrder::Lead)
            .unwrap();
        assert_eq!(
            engine.state.players["A"].discard.len(),
            1,
            "wrong dir: stays"
        );

        // The `Yours` call (your card was stopped) fires it: the card resurrects itself.
        let mut engine = engine_with(json!([]));
        engine.state.players.get_mut("A").unwrap().discard = vec![watcher("w")];
        engine
            .run_on_stop_gimmicks("A", Direction::Yours, PlayOrder::Lead)
            .unwrap();
        assert_eq!(
            engine.state.players["A"].discard.len(),
            0,
            "left the discard"
        );
        assert_eq!(
            engine.state.players["A"].hand.len(),
            1,
            "resurrected to hand"
        );
    }

    /// A `WhileInDiscard` OnBreakout self-resurrect (task #115 slice 2b): "When this card
    /// is in your discard pile and either player breaks out, shuffle it into your deck."
    /// Fires EXACTLY ONCE — the standing scan and the discard self-trigger scan are split
    /// so the effect no longer double-fires (once flat via `triggered_effects`, once
    /// bound) — and `self_card` binds so the right card leaves the discard.
    #[test]
    fn while_in_discard_onbreakout_fires_once_and_self_resurrects() {
        let watcher: Card = serde_json::from_value(json!({
            "atk_type":"Grapple","db_uuid":"recur","name":"Comeback","number":30,
            "play_order":"Lead","raw_text":"","tags":[],"finish_bonuses":{},
            "effects":[{"@type":"Effect",
                "trigger":{"@type":"OnBreakout","who":null},
                "condition":{"@type":"Always"},
                "actions":[{"@type":"ShuffleSelfIntoDeck"}],
                "duration":"WHILE_IN_DISCARD","optional":false,
                "frequency":{"@type":"FrequencyGuard","kind":"UNLIMITED","n":null},
                "raw_clause":"","source":"card"}]
        }))
        .unwrap();

        let mut engine = engine_with(json!([]));
        engine.state.players.get_mut("A").unwrap().discard = vec![watcher];
        engine.on_broken_out("B").unwrap(); // B finished; A (breaker) broke out
        assert_eq!(
            engine.state.players["A"].discard.len(),
            0,
            "left the discard"
        );
        assert_eq!(
            engine.state.players["A"]
                .deck
                .iter()
                .filter(|c| c.db_uuid == "recur")
                .count(),
            1,
            "shuffled itself in exactly once (no double-fire)"
        );
    }

    /// `DoubleFinishIf{condition}` (schema v77) doubles a card's own printed Finish
    /// bonus when the condition holds — Kenzie Cutter: doubled only with another
    /// Submission in play (`HasInPlay{Submission, >=2}`).
    #[test]
    fn double_finish_if_doubles_only_when_condition_holds() {
        let cutter: Card = serde_json::from_value(json!({
            "atk_type":"Submission","db_uuid":"cutter","name":"Kenzie Cutter","number":30,
            "play_order":"Finish","raw_text":"","tags":[],"finish_bonuses":{"Power":2},
            "effects":[{"@type":"Effect","trigger":{"@type":"Static"},"condition":{"@type":"Always"},
                "actions":[{"@type":"DoubleFinishIf","condition":{"@type":"HasInPlay","who":"SELF",
                    "count":2,"cmp":">=","filter":{"@type":"CardFilter","atk_type":"Submission"}}}],
                "duration":"WHILE_IN_PLAY","optional":false,
                "frequency":{"@type":"FrequencyGuard","kind":"UNLIMITED","n":null},
                "raw_clause":"","source":"card"}]
        }))
        .unwrap();
        let other: Card = serde_json::from_value(json!({"atk_type":"Submission","db_uuid":"o",
            "name":"o","number":1,"play_order":"Finish","raw_text":"","tags":[],
            "finish_bonuses":{},"effects":[]}))
        .unwrap();
        let mut engine = engine_with(json!([]));

        engine.state.players.get_mut("A").unwrap().in_play = vec![cutter.clone()];
        assert_eq!(
            engine.card_finish_bonus(&cutter, Skill::Power, "A"),
            2,
            "only 1 Submission in play → not doubled"
        );

        engine.state.players.get_mut("A").unwrap().in_play = vec![cutter.clone(), other];
        assert_eq!(
            engine.card_finish_bonus(&cutter, Skill::Power, "A"),
            4,
            "another Submission in play → doubled"
        );
    }

    #[test]
    fn ended_turn_no_play_gates_the_boss_finish_double() {
        // The SRG Boss "Throw in the Towels": double the finish bonus if you ended the
        // last turn without playing a card (flags["last_pass_turn"] == turn_no - 1).
        let towels: Card = serde_json::from_value(json!({
            "atk_type":"Submission","db_uuid":"towels","name":"Throw in the Towels","number":30,
            "play_order":"Finish","raw_text":"","tags":[],"finish_bonuses":{"Submission":1},
            "effects":[{"@type":"Effect","trigger":{"@type":"Static"},"condition":{"@type":"Always"},
                "actions":[{"@type":"DoubleFinishIf","condition":{"@type":"EndedTurnNoPlay"}}],
                "duration":"WHILE_IN_PLAY","optional":false,
                "frequency":{"@type":"FrequencyGuard","kind":"UNLIMITED","n":null},
                "raw_clause":"","source":"card"}]
        }))
        .unwrap();
        let mut engine = engine_with(json!([]));
        engine.state.turn_no = 5;
        engine.state.players.get_mut("A").unwrap().in_play = vec![towels.clone()];

        // Played (or lost the roll) last turn → not doubled.
        assert_eq!(
            engine.card_finish_bonus(&towels, Skill::Submission, "A"),
            1,
            "no recorded pass on turn 4 → EndedTurnNoPlay false → not doubled"
        );

        // Passed on the immediately-previous turn → doubled.
        engine
            .state
            .players
            .get_mut("A")
            .unwrap()
            .flags
            .insert("last_pass_turn".to_owned(), json!(4));
        assert_eq!(
            engine.card_finish_bonus(&towels, Skill::Submission, "A"),
            2,
            "passed on turn 4 (turn_no-1) → EndedTurnNoPlay true → doubled"
        );

        // A pass two turns ago does not count — only the immediately previous turn.
        engine
            .state
            .players
            .get_mut("A")
            .unwrap()
            .flags
            .insert("last_pass_turn".to_owned(), json!(3));
        assert_eq!(
            engine.card_finish_bonus(&towels, Skill::Submission, "A"),
            1,
            "pass on turn 3 (not turn_no-1) → not doubled"
        );
    }

    /// King Cage Clutch: "double these bonuses if you rolled Power for your turn roll
    /// or re-rolled your turn roll" — `DoubleFinishIf{Or{RollWasSkill{Power}, RerolledTurnRoll}}`.
    /// Either disjunct alone doubles; neither leaves the base bonus.
    #[test]
    fn king_cage_double_fires_on_rolled_power_or_a_reroll() {
        let clutch: Card = serde_json::from_value(json!({
            "atk_type":"Submission","db_uuid":"clutch","name":"King Cage Clutch","number":30,
            "play_order":"Finish","raw_text":"","tags":[],"finish_bonuses":{"Submission":3},
            "effects":[{"@type":"Effect","trigger":{"@type":"Static"},"condition":{"@type":"Always"},
                "actions":[{"@type":"DoubleFinishIf","condition":{"@type":"Or","items":[
                    {"@type":"RollWasSkill","skill":"Power","who":"SELF"},
                    {"@type":"RerolledTurnRoll"}]}}],
                "duration":"WHILE_IN_PLAY","optional":false,
                "frequency":{"@type":"FrequencyGuard","kind":"UNLIMITED","n":null},
                "raw_clause":"","source":"card"}]
        }))
        .unwrap();
        let mut engine = engine_with(json!([]));
        engine.state.turn_no = 4;
        engine.state.players.get_mut("A").unwrap().in_play = vec![clutch.clone()];

        // Neither disjunct: rolled Agility, no reroll this turn → not doubled.
        engine.roll_ctx.insert(
            "A".into(),
            RollContext {
                skill: Some(Skill::Agility),
                gap: None,
                value: None,
                opp_skill: None,
            },
        );
        assert_eq!(
            engine.card_finish_bonus(&clutch, Skill::Submission, "A"),
            3,
            "no trigger → base"
        );

        // Rolled Power → doubled.
        engine.roll_ctx.insert(
            "A".into(),
            RollContext {
                skill: Some(Skill::Power),
                gap: None,
                value: None,
                opp_skill: None,
            },
        );
        assert_eq!(
            engine.card_finish_bonus(&clutch, Skill::Submission, "A"),
            6,
            "rolled Power → doubled"
        );

        // Rolled Agility but re-rolled this turn (flag stamped) → doubled via the OR.
        engine.roll_ctx.insert(
            "A".into(),
            RollContext {
                skill: Some(Skill::Agility),
                gap: None,
                value: None,
                opp_skill: None,
            },
        );
        engine
            .state
            .players
            .get_mut("A")
            .unwrap()
            .flags
            .insert("rerolled_turn".to_owned(), json!(4));
        assert_eq!(
            engine.card_finish_bonus(&clutch, Skill::Submission, "A"),
            6,
            "re-rolled → doubled"
        );
    }

    /// Foxworthy V3 Bell Cracker: "if you have 0 cards in your deck, double these
    /// bonuses" — `DoubleFinishIf{DeckSizeCompare{=, 0, SELF}}`.
    #[test]
    fn foxworthy_double_fires_only_on_an_empty_deck() {
        let bell: Card = serde_json::from_value(json!({
            "atk_type":"Grapple","db_uuid":"bell","name":"Bell Cracker","number":29,
            "play_order":"Finish","raw_text":"","tags":[],"finish_bonuses":{"Agility":2},
            "effects":[{"@type":"Effect","trigger":{"@type":"Static"},"condition":{"@type":"Always"},
                "actions":[{"@type":"DoubleFinishIf","condition":{"@type":"DeckSizeCompare",
                    "cmp":"=","value":0,"who":"SELF"}}],
                "duration":"WHILE_IN_PLAY","optional":false,
                "frequency":{"@type":"FrequencyGuard","kind":"UNLIMITED","n":null},
                "raw_clause":"","source":"card"}]
        }))
        .unwrap();
        let filler: Card = serde_json::from_value(json!({"atk_type":"Strike","db_uuid":"f",
            "name":"f","number":1,"play_order":"Lead","raw_text":"","tags":[],
            "finish_bonuses":{},"effects":[]}))
        .unwrap();
        let mut engine = engine_with(json!([]));
        engine.state.players.get_mut("A").unwrap().in_play = vec![bell.clone()];

        engine.state.players.get_mut("A").unwrap().deck = vec![filler];
        assert_eq!(
            engine.card_finish_bonus(&bell, Skill::Agility, "A"),
            2,
            "deck not empty → not doubled"
        );

        engine.state.players.get_mut("A").unwrap().deck = vec![];
        assert_eq!(
            engine.card_finish_bonus(&bell, Skill::Agility, "A"),
            4,
            "0 cards in deck → doubled"
        );
    }
}

#[cfg(test)]
mod require_stops_tests {
    use super::*;
    use serde_json::{json, Value};

    fn finish_stop(number: i64) -> Card {
        serde_json::from_value(json!({
            "atk_type": "Strike", "db_uuid": format!("stop{number}"), "name": "stop",
            "number": number, "play_order": "Lead", "raw_text": "", "tags": [],
            "finish_bonuses": {},
            "effects": [{
                "@type": "Effect", "trigger": {"@type": "Static"}, "condition": {"@type": "Always"},
                "actions": [{"@type": "Stop", "order": "Finish", "atk_type": null,
                             "source_is_skillreq": false}],
                "duration": "INSTANT",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "stop", "source": "card", "optional": false
            }]
        }))
        .expect("stop")
    }

    /// A Finish attack that "can only be stopped by `count` Stops".
    fn require_stops_finish(count: i64) -> Card {
        serde_json::from_value(json!({
            "atk_type": "Grapple", "db_uuid": "combo", "name": "Killer Combo", "number": 28,
            "play_order": "Finish", "raw_text": "", "tags": [], "finish_bonuses": {},
            "effects": [{
                "@type": "Effect", "trigger": {"@type": "Static"}, "condition": {"@type": "Always"},
                "actions": [{"@type": "RequireStops", "count": count}],
                "duration": "WHILE_IN_PLAY",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "2 stops", "source": "card", "optional": false
            }]
        }))
        .expect("attack")
    }

    /// Picks the first non-"none" (a stop) option; falls back to the first option.
    struct PickStop;
    impl Decider for PickStop {
        fn decide(
            &mut self,
            _: &str,
            _: &str,
            legal: &[Value],
            _: &mut GameState,
        ) -> Option<Value> {
            legal
                .iter()
                .find(|o| o["kind"] != "none")
                .cloned()
                .or_else(|| legal.first().cloned())
        }
        fn policy_name(&self, _: &str) -> String {
            "pick".into()
        }
    }

    fn engine() -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let deck = |id: &str| -> Deck {
            serde_json::from_value(json!({
                "competitor": {"db_uuid": id, "name": id, "division": "World Championship",
                    "stats": stats},
                "entrance": {"db_uuid": format!("{id}-ent"), "name": "ent"}, "cards": [],
            }))
            .expect("deck")
        };
        Engine::new(
            deck("A"),
            deck("B"),
            Box::new(PickStop),
            1,
            String::new(),
            "sim".into(),
        )
    }

    #[test]
    fn stops_required_reads_the_count() {
        assert_eq!(Engine::stops_required(&require_stops_finish(2)), 2);
        assert_eq!(Engine::stops_required(&require_stops_finish(1)), 1);
        // a plain attack with no RequireStops needs a single stop
        let plain: Card = serde_json::from_value(json!({"atk_type":"Strike","db_uuid":"p",
            "name":"p","number":1,"play_order":"Finish","raw_text":"","tags":[],
            "finish_bonuses":{},"effects":[]}))
        .unwrap();
        assert_eq!(Engine::stops_required(&plain), 1);
    }

    #[test]
    fn require_two_stops_is_unstoppable_with_one_in_hand() {
        let mut e = engine();
        e.state.players.get_mut("B").unwrap().hand = vec![finish_stop(1)];
        // one legal stop < the 2 required → not offered, the finish lands
        assert!(e
            .offer_stop("B", &require_stops_finish(2))
            .unwrap()
            .is_none());
    }

    #[test]
    fn require_two_stops_commits_two_when_available() {
        let mut e = engine();
        e.state.players.get_mut("B").unwrap().hand =
            vec![finish_stop(1), finish_stop(2), finish_stop(3)];
        let (_, extra) = e
            .offer_stop("B", &require_stops_finish(2))
            .unwrap()
            .expect("2 legal stops → stoppable");
        assert_eq!(
            extra.len(),
            1,
            "a 2-stop finish consumes exactly one extra stop"
        );
        assert_eq!(
            e.state.players["B"].hand.len(),
            1,
            "both committed stops left the defender's hand (3 → 1)"
        );
    }

    /// A Grapple-typed Finish stop with deck `number`.
    fn grapple_finish_stop(number: i64) -> Card {
        serde_json::from_value(json!({
            "atk_type": "Grapple", "db_uuid": format!("gstop{number}"), "name": "gstop",
            "number": number, "play_order": "Lead", "raw_text": "", "tags": [],
            "finish_bonuses": {},
            "effects": [{
                "@type": "Effect", "trigger": {"@type": "Static"}, "condition": {"@type": "Always"},
                "actions": [{"@type": "Stop", "order": "Finish", "atk_type": "Grapple",
                             "source_is_skillreq": false}],
                "duration": "INSTANT",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "stop", "source": "card", "optional": false
            }]
        }))
        .expect("gstop")
    }

    /// A Strike Finish; `also_grapple` adds an `AlsoAtkType{Grapple}` alias.
    fn strike_finish(also_grapple: bool) -> Card {
        let effects = if also_grapple {
            json!([{
                "@type": "Effect", "trigger": {"@type": "Static"}, "condition": {"@type": "Always"},
                "actions": [{"@type": "AlsoAtkType", "atk_type": "Grapple"}],
                "duration": "WHILE_IN_PLAY",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "also grapple", "source": "card", "optional": false
            }])
        } else {
            json!([])
        };
        serde_json::from_value(json!({
            "atk_type": "Strike", "db_uuid": "sfin", "name": "sfin", "number": 28,
            "play_order": "Finish", "raw_text": "", "tags": [], "finish_bonuses": {},
            "effects": effects
        }))
        .expect("strike finish")
    }

    #[test]
    fn counts_as_atk_type_reads_the_alias() {
        let atk = strike_finish(true);
        assert!(
            atk.counts_as_atk_type(AtkType::Strike),
            "printed type still counts"
        );
        assert!(
            atk.counts_as_atk_type(AtkType::Grapple),
            "aliased type counts"
        );
        assert!(
            !atk.counts_as_atk_type(AtkType::Submission),
            "an unrelated type does not"
        );
        assert!(
            !strike_finish(false).counts_as_atk_type(AtkType::Grapple),
            "no alias → no Grapple"
        );
    }

    #[test]
    fn also_atk_type_lets_a_grapple_stop_catch_a_strike_finish() {
        let e = engine();
        // "Also a Finish Grapple" → a Grapple-typed Finish stop now matches.
        assert!(e.card_can_stop("B", &grapple_finish_stop(1), &strike_finish(true)));
        // Without the alias, a Grapple stop cannot catch a plain Strike finish.
        assert!(!e.card_can_stop("B", &grapple_finish_stop(1), &strike_finish(false)));
    }
}

#[cfg(test)]
mod cardona_mechanism_tests {
    use super::*;
    use serde_json::{json, Value};

    struct PickFirst;
    impl Decider for PickFirst {
        fn decide(
            &mut self,
            _: &str,
            _: &str,
            legal: &[Value],
            _: &mut GameState,
        ) -> Option<Value> {
            legal.first().cloned()
        }
        fn policy_name(&self, _: &str) -> String {
            "pick".into()
        }
    }

    /// Declines every "you may" (picks the `kind:"no"` option), else falls back to the
    /// first legal choice.
    struct PickDecline;
    impl Decider for PickDecline {
        fn decide(
            &mut self,
            _: &str,
            _: &str,
            legal: &[Value],
            _: &mut GameState,
        ) -> Option<Value> {
            legal
                .iter()
                .find(|o| o.get("kind").and_then(Value::as_str) == Some("no"))
                .or_else(|| legal.first())
                .cloned()
        }
        fn policy_name(&self, _: &str) -> String {
            "decline".into()
        }
    }

    fn card(uuid: &str, order: &str) -> Card {
        serde_json::from_value(
            json!({"atk_type":"Strike","db_uuid":uuid,"name":uuid,"number":1,
            "play_order":order,"raw_text":"","tags":[],"finish_bonuses":{},"effects":[]}),
        )
        .unwrap()
    }

    fn engine() -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let deck = |id: &str| -> Deck {
            serde_json::from_value(json!({
                "competitor": {"db_uuid": id, "name": id, "division": "World Championship",
                    "stats": stats},
                "entrance": {"db_uuid": format!("{id}-ent"), "name": "ent"}, "cards": [],
            }))
            .unwrap()
        };
        Engine::new(
            deck("A"),
            deck("B"),
            Box::new(PickFirst),
            1,
            String::new(),
            "sim".into(),
        )
    }

    #[test]
    fn bury_per_scales_by_leads_in_play() {
        // Cardona Radio Silence: "opponent buries 1 from hand for each Lead you have in
        // play." A has 2 Leads (+ 1 Follow Up) in play → bury 2 from B's 3-card hand.
        let mut e = engine();
        e.state.players.get_mut("A").unwrap().in_play = vec![
            card("l1", "Lead"),
            card("l2", "Lead"),
            card("f1", "Followup"),
        ];
        e.state.players.get_mut("B").unwrap().hand =
            vec![card("h1", "Lead"), card("h2", "Lead"), card("h3", "Lead")];
        let bury = Action::Bury {
            selector: CardFilter::default(),
            count: 1,
            who: Who::Opp,
            random: true,
            source: BuryFrom::Hand,
            choose: false,
            per: Some(CardFilter {
                play_order: Some(PlayOrder::Lead),
                ..Default::default()
            }),
            per_who: Who::SelfSide,
            per_zone: CountZone::InPlay,
            all: false,
        };
        e.apply_action(&bury, "A", "").unwrap();
        assert_eq!(
            e.state.players["B"].hand.len(),
            1,
            "2 Leads in play → bury 2 from the opponent's hand (3 → 1)"
        );
    }

    #[test]
    fn bury_per_scales_by_strikes_flipped_this_turn() {
        // Scott's Five Star Heart Punch: "opponent buries 1 card in their hand for each
        // Strike flipped." A flipped 3 Strikes (+ a Grapple) this turn → bury 3 from B's
        // 5-card hand. `per_zone = FlippedThisTurn` reads the finisher's flips, not the
        // board.
        let atk = |uuid: &str, atk_type: &str| -> Card {
            serde_json::from_value(json!({"atk_type": atk_type, "db_uuid": uuid, "name": uuid,
                "number": 1, "play_order": "Lead", "raw_text": "", "tags": [],
                "finish_bonuses": {}, "effects": []}))
            .unwrap()
        };
        let mut e = engine();
        e.state.players.get_mut("A").unwrap().flipped_this_turn = vec![
            atk("s1", "Strike"),
            atk("s2", "Strike"),
            atk("g1", "Grapple"),
            atk("s3", "Strike"),
        ];
        e.state.players.get_mut("B").unwrap().hand =
            (0..5).map(|i| card(&format!("h{i}"), "Lead")).collect();
        let bury = Action::Bury {
            selector: CardFilter::default(),
            count: 1,
            who: Who::Opp,
            random: true,
            source: BuryFrom::Hand,
            choose: false,
            per: Some(CardFilter {
                atk_type: Some(AtkType::Strike),
                ..Default::default()
            }),
            per_who: Who::SelfSide,
            per_zone: CountZone::FlippedThisTurn,
            all: false,
        };
        e.apply_action(&bury, "A", "").unwrap();
        assert_eq!(
            e.state.players["B"].hand.len(),
            2,
            "3 Strikes flipped → bury 3 from the opponent's 5-card hand (5 → 2)"
        );
    }

    #[test]
    fn flip_then_per_flipped_bury_fire_in_order_within_onhit() {
        // Five Star Heart Punch's real shape: a "Flip N" and the "opp buries per Strike
        // flipped" bury both fire in the OnHit phase, the Flip listed first. `run_effects`
        // preserves list order, so the flip populates `flipped_this_turn` BEFORE the bury
        // reads it — the guard against regressing the trigger to OnPlay (which fires
        // before OnHit, so it would read an empty pool and bury nothing).
        let mut e = engine();
        // A's deck holds 3 Strikes to flip; B holds 4 cards to lose from.
        e.state.players.get_mut("A").unwrap().deck =
            vec![card("d1", "Lead"), card("d2", "Lead"), card("d3", "Lead")];
        e.state.players.get_mut("B").unwrap().hand =
            (0..4).map(|i| card(&format!("h{i}"), "Lead")).collect();
        // `card()` gives atk_type Strike, so all 3 flipped cards are Strikes → bury 3.
        let effects: Vec<crate::ir::Effect> = serde_json::from_value(json!([
            {"@type": "Effect", "trigger": {"@type": "OnHit"}, "condition": {"@type": "Always"},
             "actions": [{"@type": "Flip", "n": 3, "who": "SELF"}],
             "duration": "INSTANT",
             "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
             "raw_clause": "flip", "source": "card", "optional": false},
            {"@type": "Effect", "trigger": {"@type": "OnHit"}, "condition": {"@type": "Always"},
             "actions": [{"@type": "Bury", "selector": {"@type": "CardFilter"}, "count": 1,
                 "who": "OPP", "random": true, "source": "HAND", "choose": false,
                 "per": {"@type": "CardFilter", "atk_type": "Strike"}, "per_who": "SELF",
                 "per_zone": "FLIPPED_THIS_TURN", "all": false}],
             "duration": "INSTANT",
             "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
             "raw_clause": "bury", "source": "card", "optional": false},
        ]))
        .unwrap();
        e.run_effects(&effects, "OnHit", "A", None).unwrap();
        assert_eq!(
            e.state.players["B"].hand.len(),
            1,
            "flip fires first → 3 Strikes flipped → bury 3 (B hand 4 → 1)"
        );
    }

    #[test]
    fn bury_all_clears_every_matching_hand_card() {
        // "Look at your opponent's hand, they bury all Leads." B holds 2 Leads and a
        // Follow Up → both Leads go, the Follow Up stays; count is a placeholder.
        let mut e = engine();
        e.state.players.get_mut("B").unwrap().hand = vec![
            card("l1", "Lead"),
            card("fu", "Followup"),
            card("l2", "Lead"),
        ];
        let bury = Action::Bury {
            selector: CardFilter {
                play_order: Some(PlayOrder::Lead),
                ..Default::default()
            },
            count: 0,
            who: Who::Opp,
            random: true,
            source: BuryFrom::Hand,
            choose: false,
            per: None,
            per_who: Who::SelfSide,
            per_zone: CountZone::InPlay,
            all: true,
        };
        e.apply_action(&bury, "A", "").unwrap();
        let hand = &e.state.players["B"].hand;
        assert_eq!(hand.len(), 1, "both Leads buried, Follow Up stays");
        assert_eq!(hand[0].db_uuid, "fu");
    }

    #[test]
    fn hit_card_reads_this_and_last_turn_history() {
        use crate::conditions::holds;
        let mut e = engine();
        // A hit a Lead this turn (all cards in this module are atk_type Strike).
        e.state.players.get_mut("A").unwrap().hit_this_turn = vec![card("l1", "Lead")];
        let lead_filter = CardFilter {
            play_order: Some(PlayOrder::Lead),
            ..Default::default()
        };
        let lead = Condition::HitCard {
            filter: lead_filter.clone(),
            who: Who::SelfSide,
            last_turn: false,
        };
        let lead_last = Condition::HitCard {
            filter: lead_filter,
            who: Who::SelfSide,
            last_turn: true,
        };
        assert!(holds(&lead, &e.state, "A", None), "hit a Lead this turn");
        assert!(!holds(&lead_last, &e.state, "A", None), "not yet last turn");

        // A turn boundary rotates this-turn history into last-turn.
        e.state.players.get_mut("A").unwrap().hit_last_turn =
            std::mem::take(&mut e.state.players.get_mut("A").unwrap().hit_this_turn);
        assert!(
            !holds(&lead, &e.state, "A", None),
            "cleared for the new turn"
        );
        assert!(
            holds(&lead_last, &e.state, "A", None),
            "now a last-turn hit"
        );
    }

    #[test]
    fn shuffle_into_deck_from_in_play_returns_the_followup() {
        // Cardona Re-boot: "shuffle 1 Follow Up you have in play into your deck."
        let mut e = engine();
        e.state.players.get_mut("A").unwrap().in_play =
            vec![card("lead", "Lead"), card("fu", "Followup")];
        e.state.players.get_mut("A").unwrap().deck = vec![];
        let shuffle = Action::ShuffleIntoDeck {
            selector: CardFilter {
                play_order: Some(PlayOrder::Followup),
                ..Default::default()
            },
            source: ShuffleSource::InPlay,
            all: false,
            then_draw: false,
            then_bury: false,
        };
        e.apply_action(&shuffle, "A", "").unwrap();
        assert_eq!(
            e.state.players["A"].deck.len(),
            1,
            "the Follow Up returned to the deck"
        );
        assert_eq!(e.state.players["A"].deck[0].db_uuid, "fu");
        assert_eq!(
            e.state.players["A"].in_play.len(),
            1,
            "only the Lead remains in play"
        );
        assert_eq!(e.state.players["A"].in_play[0].db_uuid, "lead");
    }

    #[test]
    fn shuffle_into_deck_all_recycles_matches_and_draws_the_same_number() {
        // AJ Styles' Spiral Tap: "Take any number of Lead cards from your discard pile
        // and shuffle them into your deck, then draw the same number of cards."
        let mut e = engine();
        {
            let a = e.state.players.get_mut("A").unwrap();
            // Discard has 2 Leads (recyclable) + 1 Follow Up (not matched).
            a.discard = vec![
                card("l1", "Lead"),
                card("fu", "Followup"),
                card("l2", "Lead"),
            ];
            a.deck = vec![card("d0", "Lead"), card("d1", "Lead"), card("d2", "Lead")];
            a.hand.clear();
        }
        let shuffle = Action::ShuffleIntoDeck {
            selector: CardFilter {
                play_order: Some(PlayOrder::Lead),
                ..Default::default()
            },
            source: ShuffleSource::Discard,
            all: true,
            then_draw: true,
            then_bury: false,
        };
        e.apply_action(&shuffle, "A", "").unwrap();
        let a = &e.state.players["A"];
        // Both Leads left the discard; the Follow Up stayed.
        assert_eq!(
            a.discard.len(),
            1,
            "only the non-Lead remains in the discard"
        );
        assert_eq!(a.discard[0].db_uuid, "fu");
        // Drew exactly the 2 that were shuffled (deck: 3 + 2 recycled − 2 drawn = 3).
        assert_eq!(a.hand.len(), 2, "drew the same number that were recycled");
        assert_eq!(
            a.deck.len(),
            3,
            "deck size is net-neutral after the recycle+draw"
        );
    }

    #[test]
    fn finish_requires_gates_the_opponents_finish_by_in_play_count() {
        // D3 (V1): "Your opponent needs 3 cards in play to hit you with a Finish."
        let mut e = engine();
        let gimmick: Effect = serde_json::from_value(json!({
            "@type":"Effect","trigger":{"@type":"Static"},"condition":{"@type":"Always"},
            "actions":[{"@type":"FinishRequires","kind":"CARDS","count":3}],
            "duration":"WHILE_IN_PLAY","optional":false,
            "frequency":{"@type":"FrequencyGuard","kind":"UNLIMITED","n":null},
            "raw_clause":"","source":"gimmick"
        }))
        .unwrap();
        e.state
            .players
            .get_mut("B")
            .unwrap()
            .competitor
            .effects
            .push(gimmick);
        // A holds a single Finish. A Finish needs a Follow Up in play (the built-in
        // default), so give A a Lead+Follow Up chain: 2 cards — the default is met but
        // D3's Cards>=3 is not.
        e.state.players.get_mut("A").unwrap().hand = vec![card("fin", "Finish")];
        e.state.players.get_mut("A").unwrap().in_play =
            vec![card("lead", "Lead"), card("fu", "Followup")];
        assert!(
            e.playable_options("A").is_empty(),
            "Finish blocked at 2 cards in play (< 3)"
        );
        // A third card in play meets the requirement.
        e.state
            .players
            .get_mut("A")
            .unwrap()
            .in_play
            .push(card("fu2", "Followup"));
        assert_eq!(
            e.playable_options("A").len(),
            1,
            "Finish playable once 3 cards are in play"
        );
        // Sanity: without the gimmick the 2-card chain already suffices (default rule).
        e.state
            .players
            .get_mut("B")
            .unwrap()
            .competitor
            .effects
            .clear();
        e.state.players.get_mut("A").unwrap().in_play.pop();
        assert_eq!(
            e.playable_options("A").len(),
            1,
            "no gimmick → default FollowUps-1 rule allows the Finish at 2 cards"
        );
    }

    #[test]
    fn hand_to_deck_top_denies_an_opponent_hand_card() {
        // D3 (V1) Claw: "Look at your opponent's hand, choose 1 card and put it on top
        // of their deck." A (actor) sends one of B's hand cards to the top of B's deck.
        let mut e = engine();
        e.state.players.get_mut("B").unwrap().hand = vec![card("h1", "Lead"), card("h2", "Lead")];
        e.state.players.get_mut("B").unwrap().deck = vec![card("d1", "Lead")];
        let act = Action::HandToDeckTop {
            who: Who::Opp,
            selector: CardFilter::default(),
        };
        e.apply_action(&act, "A", "").unwrap();
        let b = &e.state.players["B"];
        assert_eq!(b.hand.len(), 1, "one card left B's hand");
        assert_eq!(b.deck.len(), 2, "and joined B's deck");
        assert_eq!(
            b.deck[0].db_uuid, "h1",
            "the chosen card sits on top of B's deck (redraw next turn)"
        );
        assert!(
            b.hand.iter().all(|c| c.db_uuid != "h1"),
            "it is gone from the hand"
        );
    }

    #[test]
    fn on_flip_gimmick_fires_only_on_the_exact_count() {
        // Evee Laveaux: "when you flip exactly 3 cards, draw 2." OnFlip{who:SELF,count:3}.
        let mut e = engine();
        let gimmick: Effect = serde_json::from_value(json!({
            "@type":"Effect","trigger":{"@type":"OnFlip","who":"SELF","count":3},
            "condition":{"@type":"Always"},
            "actions":[{"@type":"Draw","n":2,"source":"TOP","who":"SELF","per":null,
                "per_who":"SELF","cap":null,"per_excludes_trigger":false}],
            "duration":"INSTANT","optional":false,
            "frequency":{"@type":"FrequencyGuard","kind":"UNLIMITED","n":null},
            "raw_clause":"","source":"gimmick"
        }))
        .unwrap();
        e.state
            .players
            .get_mut("A")
            .unwrap()
            .competitor
            .effects
            .push(gimmick);
        e.state.players.get_mut("A").unwrap().deck =
            (0..10).map(|i| card(&format!("d{i}"), "Lead")).collect();

        let h0 = e.state.players["A"].hand.len();
        e.act_flip(2, Who::SelfSide, "A").unwrap();
        assert_eq!(
            e.state.players["A"].hand.len(),
            h0,
            "flip 2 → gimmick silent"
        );
        e.act_flip(3, Who::SelfSide, "A").unwrap();
        assert_eq!(
            e.state.players["A"].hand.len(),
            h0 + 2,
            "flip exactly 3 → draw 2"
        );
    }

    /// A card carrying its own `OnFlip{SELF}` self-trigger with `action` ("If this card
    /// is flipped, <action>"). Used to exercise each flip self-action.
    fn flip_self_card_with(uuid: &str, action: Value) -> Card {
        serde_json::from_value(json!({"atk_type":"Strike","db_uuid":uuid,"name":uuid,
            "number":1,"play_order":"Lead","raw_text":"","tags":[],"finish_bonuses":{},
            "effects":[{"@type":"Effect",
                "trigger":{"@type":"OnFlip","who":"SELF","count":null,"on_self":true},
                "condition":{"@type":"Always"},
                "actions":[action],
                "duration":"INSTANT","optional":false,
                "frequency":{"@type":"FrequencyGuard","kind":"UNLIMITED","n":null},
                "raw_clause":"flip self-trigger","source":"card"}]}))
        .unwrap()
    }

    /// A card that adds itself to hand when flipped.
    fn flip_self_card(uuid: &str) -> Card {
        flip_self_card_with(uuid, json!({"@type":"AddSelfToHand"}))
    }

    #[test]
    fn self_flip_card_adds_itself_to_hand() {
        let mut e = engine();
        // Deck top: the self-flip card, then a plain card. Flip 2.
        e.state.players.get_mut("A").unwrap().deck =
            vec![flip_self_card("self"), card("plain", "Lead")];
        let h0 = e.state.players["A"].hand.len();
        e.act_flip(2, Who::SelfSide, "A").unwrap();
        let p = &e.state.players["A"];
        assert_eq!(p.hand.len(), h0 + 1, "the self-flip card joined the hand");
        assert!(
            p.hand.iter().any(|c| c.db_uuid == "self"),
            "the carrier is the one in hand"
        );
        assert!(
            p.discard.iter().any(|c| c.db_uuid == "plain"),
            "the plain card went to the discard"
        );
        assert!(
            !p.discard.iter().any(|c| c.db_uuid == "self"),
            "the carrier was pulled back out of the discard"
        );
    }

    #[test]
    fn self_flip_declined_leaves_the_card_in_discard() {
        // "you may" (Effect::optional): a decliner keeps it in the discard.
        let mut e = engine();
        e.decider = Box::new(PickDecline);
        let mut c = flip_self_card("opt");
        c.effects[0].optional = true;
        e.state.players.get_mut("A").unwrap().deck = vec![c];
        e.act_flip(1, Who::SelfSide, "A").unwrap();
        let p = &e.state.players["A"];
        assert!(
            p.discard.iter().any(|c| c.db_uuid == "opt"),
            "declined 'you may' → card stays in the discard"
        );
        assert!(p.hand.is_empty(), "nothing entered the hand");
    }

    #[test]
    fn self_flip_shuffles_itself_back_into_the_deck() {
        let mut e = engine();
        let c = flip_self_card_with("sh", json!({"@type":"ShuffleSelfIntoDeck"}));
        // Give A a small deck so the card has somewhere to shuffle back into.
        e.state.players.get_mut("A").unwrap().deck = vec![c, card("filler", "Lead")];
        e.act_flip(1, Who::SelfSide, "A").unwrap();
        let p = &e.state.players["A"];
        assert!(
            p.deck.iter().any(|c| c.db_uuid == "sh"),
            "the flipped card returned to the deck"
        );
        assert!(
            !p.discard.iter().any(|c| c.db_uuid == "sh"),
            "it is no longer in the discard"
        );
    }

    #[test]
    fn self_flip_plays_itself_into_play() {
        let mut e = engine();
        let c = flip_self_card_with("pl", json!({"@type":"PlaySelf"}));
        e.state.players.get_mut("A").unwrap().deck = vec![c];
        // B holds no stops, so the play resolves onto the board.
        e.act_flip(1, Who::SelfSide, "A").unwrap();
        let p = &e.state.players["A"];
        assert!(
            p.in_play.iter().any(|c| c.db_uuid == "pl"),
            "the flipped card resolved into play"
        );
        assert!(
            !p.discard.iter().any(|c| c.db_uuid == "pl"),
            "it left the discard when played"
        );
    }

    #[test]
    fn self_flip_runs_an_arbitrary_body_when_flipped() {
        // "When this card is flipped, draw 1 card." — the self-trigger carries a plain
        // grammar body (not a self-action); run_self_flips fires it like any effect.
        let mut e = engine();
        let draw = json!({"@type":"Draw","n":1,"source":"TOP","who":"SELF","per":null,
            "per_who":"SELF","cap":null,"per_excludes_trigger":false});
        e.state.players.get_mut("A").unwrap().deck = vec![
            flip_self_card_with("body", draw),
            card("d0", "Lead"),
            card("d1", "Lead"),
        ];
        let h0 = e.state.players["A"].hand.len();
        // Flip 1: mills the carrier to the discard, then its OnFlip draws the next card.
        e.act_flip(1, Who::SelfSide, "A").unwrap();
        let p = &e.state.players["A"];
        assert_eq!(p.hand.len(), h0 + 1, "the flip body drew a card");
        assert!(
            p.hand.iter().any(|c| c.db_uuid == "d0"),
            "the drawn card is the one below the milled carrier"
        );
        assert!(
            p.discard.iter().any(|c| c.db_uuid == "body"),
            "the carrier itself stayed milled (the body draws, it doesn't self-recur)"
        );
    }

    /// A flip self-trigger carrying a condition (e.g. `FlippedForGimmick`).
    fn flip_self_card_gated(uuid: &str, condition: Value) -> Card {
        serde_json::from_value(json!({"atk_type":"Strike","db_uuid":uuid,"name":uuid,
            "number":1,"play_order":"Lead","raw_text":"","tags":[],"finish_bonuses":{},
            "effects":[{"@type":"Effect",
                "trigger":{"@type":"OnFlip","who":"SELF","count":null,"on_self":true},
                "condition":condition,
                "actions":[{"@type":"AddSelfToHand"}],
                "duration":"INSTANT","optional":false,
                "frequency":{"@type":"FrequencyGuard","kind":"UNLIMITED","n":null},
                "raw_clause":"gated flip self-trigger","source":"card"}]}))
        .unwrap()
    }

    /// A flip-causing effect from a given `source` ("gimmick"/"card"), so `act_flip`
    /// records the right provenance when applied via `apply_actions`.
    fn flip_effect(source: &str) -> Effect {
        serde_json::from_value(json!({"@type":"Effect",
            "trigger":{"@type":"OnHit","order":null,"atk_type":null,"name_contains":[],
                "text_contains":[],"on_any":false,"who":"SELF"},
            "condition":{"@type":"Always"},
            "actions":[{"@type":"Flip","n":1,"who":"SELF","per":null,"per_who":"SELF",
                "until":null,"until_to_hand":false}],
            "duration":"INSTANT","optional":false,
            "frequency":{"@type":"FrequencyGuard","kind":"UNLIMITED","n":null},
            "raw_clause":"Flip 1 card","source":source}))
        .unwrap()
    }

    #[test]
    fn flipped_for_gimmick_gate_reads_flip_source() {
        // A gimmick-source flip fires the FlippedForGimmick self-trigger; a card-source
        // flip of the same card does not.
        for (source, fires) in [("gimmick", true), ("card", false)] {
            let mut e = engine();
            let c = flip_self_card_gated("g", json!({"@type":"FlippedForGimmick"}));
            e.state.players.get_mut("A").unwrap().deck = vec![c];
            e.apply_actions(&flip_effect(source), "A").unwrap();
            let p = &e.state.players["A"];
            assert_eq!(
                p.hand.iter().any(|c| c.db_uuid == "g"),
                fires,
                "source={source}: add-to-hand fired == {fires}"
            );
        }
    }

    #[test]
    fn flipped_by_name_gate_matches_the_flipping_card() {
        // The flip's source_name (set around resolve_play) drives FlippedByName; a CI
        // substring of the flipping card's name matches, a different name does not.
        for (flipper, fires) in [
            ("Set Up the Steel Chain", true),
            ("Set Up the Ladder", false),
        ] {
            let mut e = engine();
            let c = flip_self_card_gated(
                "n",
                json!({"@type":"FlippedByName","names":["Steel Chain"]}),
            );
            e.state.players.get_mut("A").unwrap().deck = vec![c];
            e.firing_card_name = Some(flipper.to_owned());
            e.act_flip(1, Who::SelfSide, "A").unwrap();
            let p = &e.state.players["A"];
            assert_eq!(
                p.hand.iter().any(|c| c.db_uuid == "n"),
                fires,
                "flipper={flipper:?}: add-to-hand fired == {fires}"
            );
        }
    }

    #[test]
    fn add_flipped_to_hand_pulls_matching_from_the_flip_pool() {
        // "Flip 3 cards, add 1 flipped Strike to your hand": the flip fills the pool,
        // then only a Strike among the flipped three is eligible.
        let mut e = engine();
        // Deck top: a Strike then two Grapples; only the Strike is eligible.
        let mk = |u: &str, atk: &str| {
            serde_json::from_value::<Card>(json!({"atk_type":atk,"db_uuid":u,"name":u,
                "number":1,"play_order":"Lead","raw_text":"","tags":[],"finish_bonuses":{},
                "effects":[]}))
            .unwrap()
        };
        e.state.players.get_mut("A").unwrap().deck =
            vec![mk("s1", "Strike"), mk("g1", "Grapple"), mk("g2", "Grapple")];
        e.act_flip(3, Who::SelfSide, "A").unwrap();
        let filter = CardFilter {
            atk_type: Some(AtkType::Strike),
            ..Default::default()
        };
        e.act_add_flipped_to_hand(Some(1), &filter, false, "A")
            .unwrap();
        let p = &e.state.players["A"];
        assert!(
            p.hand.iter().any(|c| c.db_uuid == "s1"),
            "the flipped Strike joined the hand"
        );
        assert!(
            !p.discard.iter().any(|c| c.db_uuid == "s1"),
            "it left the discard"
        );
        assert_eq!(p.discard.len(), 2, "the two Grapples stay in the discard");
    }

    #[test]
    fn add_flipped_to_hand_none_count_takes_all_matching() {
        // "add all flipped Strikes": count=None pulls every matching flipped card.
        let mut e = engine();
        let mk = |u: &str, atk: &str| {
            serde_json::from_value::<Card>(json!({"atk_type":atk,"db_uuid":u,"name":u,
                "number":1,"play_order":"Lead","raw_text":"","tags":[],"finish_bonuses":{},
                "effects":[]}))
            .unwrap()
        };
        e.state.players.get_mut("A").unwrap().deck =
            vec![mk("s1", "Strike"), mk("s2", "Strike"), mk("g1", "Grapple")];
        e.act_flip(3, Who::SelfSide, "A").unwrap();
        let filter = CardFilter {
            atk_type: Some(AtkType::Strike),
            ..Default::default()
        };
        e.act_add_flipped_to_hand(None, &filter, false, "A")
            .unwrap();
        let p = &e.state.players["A"];
        assert_eq!(
            p.hand
                .iter()
                .filter(|c| c.atk_type == AtkType::Strike)
                .count(),
            2,
            "both flipped Strikes were added"
        );
    }

    #[test]
    fn standing_flip_trigger_fires_from_play_not_when_its_card_is_milled() {
        // A card with "when you flip any number of cards, draw 1" (standing OnFlip,
        // on_self=false). It fires when in play and a flip happens; it must NOT fire
        // merely because the card itself is milled into the discard — the on_self split.
        let standing: Effect = serde_json::from_value(json!({"@type":"Effect",
            "trigger":{"@type":"OnFlip","who":"SELF","count":null,"at_least":false,"on_self":false},
            "condition":{"@type":"Always"},
            "actions":[{"@type":"Draw","n":1,"source":"TOP","who":"SELF","per":null,
                "per_who":"SELF","cap":null,"per_excludes_trigger":false}],
            "duration":"INSTANT","optional":false,
            "frequency":{"@type":"FrequencyGuard","kind":"UNLIMITED","n":null},
            "raw_clause":"when you flip any number of cards, draw 1","source":"card"}))
        .unwrap();

        // In play: a flip fires the standing trigger -> draw 1.
        let mut e = engine();
        e.state.players.get_mut("A").unwrap().in_play =
            vec![
                serde_json::from_value(json!({"atk_type":"Strike","db_uuid":"src","name":"src",
                "number":1,"play_order":"Lead","raw_text":"","tags":[],"finish_bonuses":{},
                "effects":[standing.clone()]}))
                .unwrap(),
            ];
        e.state.players.get_mut("A").unwrap().deck =
            (0..5).map(|i| card(&format!("d{i}"), "Lead")).collect();
        let h0 = e.state.players["A"].hand.len();
        e.act_flip(1, Who::SelfSide, "A").unwrap();
        assert_eq!(
            e.state.players["A"].hand.len(),
            h0 + 1, // the flip mills from the deck; the Draw adds 1 to hand
            "standing trigger drew 1 when a flip happened in play"
        );

        // Milled: the SAME card sitting in the deck, flipped into discard, must NOT fire.
        let mut e = engine();
        e.state.players.get_mut("A").unwrap().deck = vec![serde_json::from_value(
            json!({"atk_type":"Strike","db_uuid":"src","name":"src","number":1,
                "play_order":"Lead","raw_text":"","tags":[],"finish_bonuses":{},
                "effects":[standing]}),
        )
        .unwrap()];
        let h0 = e.state.players["A"].hand.len();
        e.act_flip(1, Who::SelfSide, "A").unwrap();
        assert_eq!(
            e.state.players["A"].hand.len(),
            h0,
            "a standing 'when you flip' trigger does not fire from being milled"
        );
    }

    #[test]
    fn from_hand_reactive_boosts_breakout_on_an_opponent_finish_hit() {
        // The Mailman Always Delivers, in B's HAND: when A hits a Finish, B reveals +
        // shuffles it away for +1 to B's breakout rolls until end of turn.
        let mut e = engine();
        let mailman: Card = serde_json::from_value(json!({
            "atk_type":"Strike","db_uuid":"mail","name":"Mailman","number":28,
            "play_order":"Finish","raw_text":"","tags":[],"finish_bonuses":{},
            "effects":[{"@type":"Effect",
                "trigger":{"@type":"OnHit","who":"OPP","order":"Finish","from_hand":true},
                "condition":{"@type":"Always"},
                "actions":[{"@type":"ShuffleSelfIntoDeck"},
                           {"@type":"GrantBreakoutBonus","delta":1}],
                "duration":"INSTANT","optional":false,
                "frequency":{"@type":"FrequencyGuard","kind":"UNLIMITED","n":null},
                "raw_clause":"","source":"card"}]}))
        .unwrap();
        e.state.players.get_mut("B").unwrap().hand = vec![mailman];
        // A (the opponent) hits a Finish.
        e.run_hit_gimmicks(&card("kill", "Finish"), "A").unwrap();
        let b = &e.state.players["B"];
        assert!(b.hand.is_empty(), "Mailman revealed + shuffled out of hand");
        assert!(b.deck.iter().any(|c| c.db_uuid == "mail"), "into B's deck");
        assert_eq!(b.breakout_bonus_eot, 1, "banked +1 breakout bonus");
        assert_eq!(
            e.breakout_bonus("B", 1, Skill::Grapple),
            1,
            "the timed bonus lands on B's breakout roll"
        );
    }

    #[test]
    fn ordinal_breakout_roll_gate_fires_only_on_matched_attempts() {
        // Return to Sender #30: "when your opponent rolls their 1st or 2nd breakout roll,
        // …" — an OnBreakoutRoll{attempts:[1,2]} on B watching A's rolls.
        let mut e = engine();
        let watcher: Card = serde_json::from_value(json!({
            "atk_type":"Submission","db_uuid":"ret","name":"Return","number":30,
            "play_order":"Finish","raw_text":"","tags":[],"finish_bonuses":{},
            "effects":[{"@type":"Effect",
                "trigger":{"@type":"OnBreakoutRoll","who":"OPP","attempts":[1,2]},
                "condition":{"@type":"Always"},
                "actions":[{"@type":"Draw","n":1,"source":"TOP","who":"SELF","per":null,
                    "per_who":"SELF","cap":null,"per_excludes_trigger":false}],
                "duration":"WHILE_IN_PLAY","optional":false,
                "frequency":{"@type":"FrequencyGuard","kind":"UNLIMITED","n":null},
                "raw_clause":"","source":"card"}]}))
        .unwrap();
        e.state.players.get_mut("B").unwrap().in_play = vec![watcher];
        e.state.players.get_mut("B").unwrap().deck =
            (0..5).map(|i| card(&format!("d{i}"), "Lead")).collect();
        let h0 = e.state.players["B"].hand.len();
        // A's 3rd breakout roll — outside [1, 2], gate closed.
        e.run_on_breakout_roll("A", Skill::Grapple, 5, 3).unwrap();
        assert_eq!(e.state.players["B"].hand.len(), h0, "3rd roll → no fire");
        // A's 1st breakout roll — fires, B draws 1.
        e.run_on_breakout_roll("A", Skill::Grapple, 5, 1).unwrap();
        assert_eq!(
            e.state.players["B"].hand.len(),
            h0 + 1,
            "1st roll → B draws"
        );
    }

    #[test]
    fn ondraw_recur_pulls_the_card_from_discard_after_a_draw() {
        // The Gobstopper, in B's discard: "if you drew 1+ cards this turn, add this card
        // to your hand." Drawing bumps drew_this_turn and fires the OnDraw recur.
        let mut e = engine();
        let gob: Card = serde_json::from_value(json!({
            "atk_type":"Strike","db_uuid":"gob","name":"Gobstopper","number":28,
            "play_order":"Finish","raw_text":"","tags":[],"finish_bonuses":{},
            "effects":[{"@type":"Effect",
                "trigger":{"@type":"OnDraw","who":"SELF"},
                "condition":{"@type":"DrewThisTurn","who":"SELF","at_least":1},
                "actions":[{"@type":"AddSelfToHand"}],
                "duration":"WHILE_IN_DISCARD","optional":false,
                "frequency":{"@type":"FrequencyGuard","kind":"UNLIMITED","n":null},
                "raw_clause":"","source":"card"}]}))
        .unwrap();
        e.state.players.get_mut("B").unwrap().discard = vec![gob];
        e.state.players.get_mut("B").unwrap().deck =
            (0..3).map(|i| card(&format!("d{i}"), "Lead")).collect();
        e.draw("B", 1, DeckEnd::Top).unwrap();
        assert_eq!(e.state.players["B"].drew_this_turn, 1, "draw counted");
        assert!(
            e.state.players["B"].hand.iter().any(|c| c.db_uuid == "gob"),
            "Gobstopper recurred to hand"
        );
        assert!(
            e.state.players["B"].discard.is_empty(),
            "and left the discard"
        );
    }

    #[test]
    fn shuffle_hand_into_deck_then_draws_the_same_number() {
        // The Dudebuster: shuffle the whole hand into the deck, then draw that many back.
        let mut e = engine();
        e.state.players.get_mut("B").unwrap().hand = vec![card("h1", "Lead"), card("h2", "Lead")];
        e.state.players.get_mut("B").unwrap().deck = vec![];
        e.act_shuffle_into_deck(
            &CardFilter::default(),
            ShuffleSource::Hand,
            true,
            true,
            false,
            "B",
        )
        .unwrap();
        // The 2 hand cards were the only deck cards, so drawing 2 refills the hand.
        assert_eq!(e.state.players["B"].hand.len(), 2, "hand refilled to 2");
        assert!(e.state.players["B"].deck.is_empty(), "deck drawn down");
    }

    #[test]
    fn shuffle_discard_then_buries_the_same_number_from_hand() {
        // Double Leg Death Lock: shuffle all of discard into the deck, then bury that many
        // from hand.
        let mut e = engine();
        e.state.players.get_mut("B").unwrap().discard =
            vec![card("d1", "Lead"), card("d2", "Lead")];
        e.state.players.get_mut("B").unwrap().hand =
            vec![card("h1", "Lead"), card("h2", "Lead"), card("h3", "Lead")];
        e.act_shuffle_into_deck(
            &CardFilter::default(),
            ShuffleSource::Discard,
            true,
            false,
            true,
            "B",
        )
        .unwrap();
        let b = &e.state.players["B"];
        assert_eq!(b.hand.len(), 1, "2 of 3 hand cards buried");
        // Shuffle put the 2 discard cards into the deck; bury sent 2 hand cards to the
        // deck bottom — deck holds all 4, discard is empty.
        assert_eq!(b.deck.len(), 4, "2 shuffled + 2 buried into the deck");
        assert!(b.discard.is_empty(), "discard emptied by the shuffle");
        let buried = ["h1", "h2", "h3"]
            .iter()
            .filter(|u| b.deck.iter().any(|c| &c.db_uuid == *u))
            .count();
        assert_eq!(buried, 2, "2 hand cards were buried into the deck");
    }
}

#[cfg(test)]
mod bury_opp_discard_tests {
    use super::*;
    use crate::policy::{HeuristicPolicy, Policies};
    use serde_json::json;

    fn card(uuid: &str, order: &str, number: i64) -> Card {
        serde_json::from_value(json!({"atk_type": "Strike", "db_uuid": uuid, "name": uuid,
            "number": number, "play_order": order, "raw_text": "", "tags": [],
            "finish_bonuses": {}, "effects": []}))
        .unwrap()
    }

    fn engine() -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let deck = |id: &str| -> Deck {
            serde_json::from_value(json!({
                "competitor": {"db_uuid": id, "name": id, "division": "World Championship",
                    "stats": stats, "effects": []},
                "entrance": {"db_uuid": format!("{id}-ent"), "name": "ent"}, "cards": [],
            }))
            .expect("deck")
        };
        let pair = Policies::new(
            Box::new(HeuristicPolicy::heuristic()),
            Box::new(HeuristicPolicy::heuristic()),
        );
        Engine::new(
            deck("A"),
            deck("B"),
            Box::new(pair),
            1,
            String::new(),
            "sim".into(),
        )
    }

    fn any_filter() -> CardFilter {
        serde_json::from_value(
            json!({"@type": "CardFilter", "number": null, "atk_type": null,
            "play_order": null, "play_orders": [], "tag": null, "name": null, "raw": null,
            "name_contains": [], "text_contains": []}),
        )
        .unwrap()
    }

    #[test]
    fn burying_the_opponents_discard_does_not_panic() {
        // Regression: the heuristic `at_bury` used to look the chosen card up in the
        // ACTOR's discard, panicking when burying the OPPONENT's ("chosen card is in
        // discard"). A buries 1 from B's discard; the card must move to B's deck bottom.
        let mut engine = engine();
        engine.state.players.get_mut("B").unwrap().discard =
            vec![card("b-fin", "Finish", 20), card("b-lead", "Lead", 1)];
        let spec = BurySpec {
            selector: any_filter(),
            count: 1,
            who: Who::Opp,
            random: false,
            source: BuryFrom::Discard,
            choose: false,
        };
        engine.act_bury(spec, "A").unwrap(); // must not panic
        let b = &engine.state.players["B"];
        assert_eq!(b.discard.len(), 1, "one card left B's discard");
        assert_eq!(b.deck.len(), 1, "one card reached B's deck bottom");
        // The Finish (most recyclable) is the one buried.
        assert!(b.deck.iter().any(|c| c.db_uuid == "b-fin"));
    }
}

#[cfg(test)]
mod flip_percount_tests {
    use super::*;
    use crate::policy::{HeuristicPolicy, Policies};
    use serde_json::json;

    fn card(uuid: &str, order: &str) -> Card {
        serde_json::from_value(json!({"atk_type": "Strike", "db_uuid": uuid, "name": uuid,
            "number": 1, "play_order": order, "raw_text": "", "tags": [],
            "finish_bonuses": {}, "effects": []}))
        .unwrap()
    }

    fn engine() -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let deck = |id: &str| -> Deck {
            serde_json::from_value(json!({
                "competitor": {"db_uuid": id, "name": id, "division": "World Championship",
                    "stats": stats, "effects": []},
                "entrance": {"db_uuid": format!("{id}-ent"), "name": "ent"}, "cards": [],
            }))
            .expect("deck")
        };
        let pair = Policies::new(
            Box::new(HeuristicPolicy::heuristic()),
            Box::new(HeuristicPolicy::heuristic()),
        );
        Engine::new(
            deck("A"),
            deck("B"),
            Box::new(pair),
            1,
            String::new(),
            "sim".into(),
        )
    }

    fn lead_filter() -> Value {
        json!({"@type": "CardFilter", "number": null, "atk_type": null,
            "play_order": "Lead", "play_orders": [], "tag": null, "name": null, "raw": null,
            "name_contains": [], "text_contains": []})
    }

    /// "Flip 1 card for each Lead you have in play": milling scales by the count of
    /// A's own Leads, and the flipped cards land in A's discard.
    #[test]
    fn per_count_flip_scales_the_mill_by_board_count() {
        let mut engine = engine();
        {
            let a = engine.state.players.get_mut("A").unwrap();
            a.deck = (0..10).map(|i| card(&format!("d{i}"), "Finish")).collect();
            a.in_play = vec![
                card("l0", "Lead"),
                card("l1", "Lead"),
                card("f0", "Followup"),
            ];
        }
        let flip: Action = serde_json::from_value(json!({
            "@type": "Flip", "n": 1, "who": "SELF", "per": lead_filter(), "per_who": "SELF"
        }))
        .unwrap();
        engine.apply_action(&flip, "A", "").unwrap();
        let a = &engine.state.players["A"];
        assert_eq!(a.discard.len(), 2, "two Leads -> two cards milled");
        assert_eq!(a.deck.len(), 8, "deck shrank by the same two");
    }

    /// A zero board count flips nothing (0 * n = 0), leaving the deck intact.
    #[test]
    fn per_count_flip_with_no_matches_mills_nothing() {
        let mut engine = engine();
        {
            let a = engine.state.players.get_mut("A").unwrap();
            a.deck = (0..5).map(|i| card(&format!("d{i}"), "Finish")).collect();
            a.in_play = vec![card("f0", "Followup")];
        }
        let flip: Action = serde_json::from_value(json!({
            "@type": "Flip", "n": 2, "who": "SELF", "per": lead_filter(), "per_who": "SELF"
        }))
        .unwrap();
        engine.apply_action(&flip, "A", "").unwrap();
        assert_eq!(
            engine.state.players["A"].deck.len(),
            5,
            "no Leads -> no mill"
        );
        assert!(engine.state.players["A"].discard.is_empty());
    }

    fn atk_card(uuid: &str, atk: &str) -> Card {
        serde_json::from_value(json!({"atk_type": atk, "db_uuid": uuid, "name": uuid,
            "number": 1, "play_order": "Lead", "raw_text": "", "tags": [],
            "finish_bonuses": {}, "effects": []}))
        .unwrap()
    }

    fn sub_filter() -> Value {
        json!({"@type": "CardFilter", "number": null, "atk_type": "Submission",
            "play_order": null, "play_orders": [], "tag": null, "name": null, "raw": null,
            "name_contains": [], "text_contains": []})
    }

    /// "Flip cards until you flip a Submission, add that Submission to your hand":
    /// mills the non-matching prefix to discard and tutors the matching card to hand.
    #[test]
    fn flip_until_adds_the_matching_card_to_hand() {
        let mut engine = engine();
        {
            let a = engine.state.players.get_mut("A").unwrap();
            a.deck = vec![
                atk_card("s0", "Strike"),
                atk_card("s1", "Strike"),
                atk_card("sub", "Submission"),
                atk_card("s2", "Strike"),
            ];
            a.hand.clear();
        }
        let flip: Action = serde_json::from_value(json!({
            "@type": "Flip", "n": 0, "who": "SELF", "per": null, "per_who": "SELF",
            "until": sub_filter(), "until_to_hand": true
        }))
        .unwrap();
        engine.apply_action(&flip, "A", "").unwrap();
        let a = &engine.state.players["A"];
        assert_eq!(a.deck.len(), 1, "stops at the Submission; s2 stays in deck");
        assert_eq!(a.deck[0].db_uuid, "s2");
        assert_eq!(a.discard.len(), 2, "the two Strikes milled");
        assert_eq!(a.hand.len(), 1, "the Submission tutored to hand");
        assert_eq!(a.hand[0].db_uuid, "sub");
    }

    /// Without `until_to_hand`, the matching card mills to discard with the rest.
    #[test]
    fn flip_until_without_add_mills_the_match_too() {
        let mut engine = engine();
        {
            let a = engine.state.players.get_mut("A").unwrap();
            a.deck = vec![
                atk_card("s0", "Strike"),
                atk_card("sub", "Submission"),
                atk_card("s1", "Strike"),
            ];
            a.hand.clear();
        }
        let flip: Action = serde_json::from_value(json!({
            "@type": "Flip", "n": 0, "who": "SELF", "per": null, "per_who": "SELF",
            "until": sub_filter(), "until_to_hand": false
        }))
        .unwrap();
        engine.apply_action(&flip, "A", "").unwrap();
        let a = &engine.state.players["A"];
        assert_eq!(
            a.deck.len(),
            1,
            "s1 stays after the Submission stops the mill"
        );
        assert_eq!(
            a.discard.len(),
            2,
            "the Strike and the Submission both milled"
        );
        assert!(a.hand.is_empty(), "nothing tutored");
    }

    /// No match: the whole deck flips to discard, and nothing is tutored even with
    /// `until_to_hand`.
    #[test]
    fn flip_until_no_match_mills_the_whole_deck() {
        let mut engine = engine();
        {
            let a = engine.state.players.get_mut("A").unwrap();
            a.deck = vec![atk_card("s0", "Strike"), atk_card("s1", "Strike")];
            a.hand.clear();
        }
        let flip: Action = serde_json::from_value(json!({
            "@type": "Flip", "n": 0, "who": "SELF", "per": null, "per_who": "SELF",
            "until": sub_filter(), "until_to_hand": true
        }))
        .unwrap();
        engine.apply_action(&flip, "A", "").unwrap();
        let a = &engine.state.players["A"];
        assert!(a.deck.is_empty(), "deck exhausted looking for a Submission");
        assert_eq!(a.discard.len(), 2, "both Strikes milled");
        assert!(a.hand.is_empty(), "no match -> nothing added");
    }

    /// "Look at the top 3 cards of your deck, add 1 to your hand and flip the
    /// others": a Scry with rest=FLIP keeps the best card and mills the leftovers
    /// to discard. The Finish (best) is the one kept.
    #[test]
    fn scry_flip_keeps_best_and_mills_the_rest() {
        let mut engine = engine();
        {
            let a = engine.state.players.get_mut("A").unwrap();
            a.deck = vec![
                atk_card("s0", "Strike"),
                card("fin", "Finish"),
                atk_card("s1", "Strike"),
                atk_card("keep-me", "Strike"),
            ];
            a.hand.clear();
        }
        let scry: Action = serde_json::from_value(json!({
            "@type": "Scry", "deck": "SELF", "top": 3, "bottom": 0, "reveal": false,
            "to_hand": 1, "bury": 0, "rest": "FLIP"
        }))
        .unwrap();
        engine.apply_action(&scry, "A", "").unwrap();
        let a = &engine.state.players["A"];
        assert_eq!(a.hand.len(), 1, "one card added to hand");
        assert_eq!(a.hand[0].db_uuid, "fin", "the Finish (best) is kept");
        assert_eq!(a.discard.len(), 2, "the two leftover Strikes milled");
        assert_eq!(a.deck.len(), 1, "the untouched 4th card stays in deck");
        assert_eq!(a.deck[0].db_uuid, "keep-me");
    }

    /// `ScryRest::MayFlip` (task #119): peek the top card of an opponent's deck and
    /// flip it only when it is worth denying them — a Finish is milled to their
    /// discard, a plain card is left on top. schema v96
    #[test]
    fn scry_may_flip_denies_a_valuable_card_but_leaves_junk() {
        let scry: Action = serde_json::from_value(json!({
            "@type": "Scry", "deck": "OPP", "top": 1, "bottom": 0, "reveal": false,
            "to_hand": 0, "bury": 0, "rest": "MAY_FLIP"
        }))
        .unwrap();

        // A valuable top card (Finish) -> milled to the opponent's discard.
        let mut eng = engine();
        {
            let b = eng.state.players.get_mut("B").unwrap();
            b.deck = vec![card("fin", "Finish"), atk_card("s1", "Strike")];
            b.discard.clear();
        }
        eng.apply_action(&scry, "A", "").unwrap();
        let b = &eng.state.players["B"];
        assert_eq!(b.discard.len(), 1, "the Finish is flipped (denied)");
        assert_eq!(b.discard[0].db_uuid, "fin");
        assert_eq!(b.deck.len(), 1, "only the single top card was peeked");
        assert_eq!(b.deck[0].db_uuid, "s1", "the junk below stays in the deck");

        // A plain top card -> not worth flipping, left on top, nothing milled.
        let mut eng = engine();
        {
            let b = eng.state.players.get_mut("B").unwrap();
            b.deck = vec![atk_card("j0", "Strike"), card("fin", "Finish")];
            b.discard.clear();
        }
        eng.apply_action(&scry, "A", "").unwrap();
        let b = &eng.state.players["B"];
        assert!(b.discard.is_empty(), "a plain card is not worth flipping");
        assert_eq!(b.deck[0].db_uuid, "j0", "it stays on top of the deck");
    }

    /// ModifyRoll `per_zone=DISCARD` scales the next-roll bonus by the count of
    /// matching cards in the DISCARD pile, not the board (Any Last Words?: "+2 for
    /// each Finish in your discard pile"). schema v70
    #[test]
    fn modify_roll_per_zone_discard_counts_the_discard_pile() {
        let mut engine = engine();
        {
            let a = engine.state.players.get_mut("A").unwrap();
            a.discard = vec![
                atk_card("f0", "Strike"), // atk_card play_order is Lead — not a Finish
                card("fin0", "Finish"),
                card("fin1", "Finish"),
            ];
            a.in_play = vec![card("board-fin", "Finish")]; // a board Finish must NOT be counted
        }
        let mr: Action = serde_json::from_value(json!({
            "@type":"ModifyRoll","who":"SELF","delta":2,"when":"NEXT",
            "per": {"@type":"CardFilter","play_order":"Finish","atk_type":null,"is_stop":null,
                "name":null,"name_contains":[],"number":null,"play_orders":[],"raw":null,"tag":null,
                "text_contains":[]},
            "per_who":"SELF","per_zone":"DISCARD"
        }))
        .unwrap();
        engine.apply_action(&mr, "A", "").unwrap();
        assert_eq!(
            engine.state.players["A"].pending_roll_mods.next_turn, 4,
            "2 Finishes in discard * +2 (the board Finish is not counted)"
        );
    }

    /// "If you have another Strike in play your next turn roll is +2" (task #131): an
    /// OnHit ModifyRoll{NEXT} gated on HasInPlay count>=2 of the attack type. The card
    /// fires OnHit (already in play), so "another" = the card plus one other Strike;
    /// the pending bonus is granted only when a second Strike is on the board.
    #[test]
    fn gated_next_turn_roll_needs_a_second_qualifier_in_play() {
        let gated: Effect = serde_json::from_value(json!({
            "@type": "Effect",
            "trigger": {"@type": "OnHit", "order": null, "atk_type": null,
                "name_contains": [], "text_contains": []},
            "condition": {"@type": "HasInPlay", "who": "SELF", "cmp": ">=", "count": 2,
                "filter": {"@type": "CardFilter", "atk_type": "Strike", "play_orders": [],
                    "is_stop": null, "name": null, "name_contains": [], "number": null,
                    "play_order": null, "raw": null, "tag": null, "text_contains": []}},
            "actions": [{"@type": "ModifyRoll", "who": "SELF", "delta": 2, "when": "NEXT",
                "per": null, "per_who": "OPP", "per_zone": "IN_PLAY"}],
            "duration": "INSTANT",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "", "source": "card", "optional": false
        }))
        .unwrap();

        // One Strike on the board (the card itself): count 1 < 2 -> no bonus.
        let mut engine = engine();
        engine.state.players.get_mut("A").unwrap().in_play = vec![atk_card("s0", "Strike")];
        engine
            .run_effects(std::slice::from_ref(&gated), "OnHit", "A", None)
            .unwrap();
        assert_eq!(
            engine.state.players["A"].pending_roll_mods.next_turn, 0,
            "a lone Strike does not arm its own 'another' gate"
        );

        // A second Strike in play: count 2 -> the +2 is granted.
        engine
            .state
            .players
            .get_mut("A")
            .unwrap()
            .in_play
            .push(atk_card("s1", "Strike"));
        engine
            .run_effects(std::slice::from_ref(&gated), "OnHit", "A", None)
            .unwrap();
        assert_eq!(
            engine.state.players["A"].pending_roll_mods.next_turn, 2,
            "a second Strike arms the next-turn-roll +2"
        );
    }

    /// `AlsoLead { order: Followup, .. }` makes a card playable as a Follow Up — only
    /// while a Lead is in play AND its condition (here, having rolled Agility this
    /// turn) holds against the current roll context. schema v70
    #[test]
    fn also_lead_followup_needs_a_lead_and_a_matching_roll() {
        let card_with: Card = serde_json::from_value(json!({
            "atk_type":"Strike","db_uuid":"ovx","name":"Overnight Express","number":16,
            "play_order":"Finish","raw_text":"","tags":[],"finish_bonuses":{},
            "effects":[{"@type":"Effect","trigger":{"@type":"Static"},"condition":{"@type":"Always"},
                "actions":[{"@type":"AlsoLead","order":"Followup",
                    "condition":{"@type":"RollWasSkill","skill":"Agility"}}],
                "duration":"WHILE_IN_PLAY","optional":false,"raw_clause":"",
                "frequency":{"@type":"FrequencyGuard","kind":"UNLIMITED","n":null},"source":"card"}]
        }))
        .unwrap();
        let mut engine = engine();
        engine.roll_ctx.insert(
            "A".into(),
            crate::conditions::RollContext {
                skill: Some(Skill::Agility),
                gap: None,
                value: None,
                opp_skill: None,
            },
        );

        // No Lead in play yet: a Follow Up slot is illegal even with the roll match.
        assert!(
            !engine.also_lead_now("A", &card_with),
            "no Lead in play → not playable"
        );

        engine.state.players.get_mut("A").unwrap().in_play = vec![card("lead", "Lead")];
        assert!(
            engine.also_lead_now("A", &card_with),
            "Lead in play + rolled Agility → playable as a Follow Up"
        );

        // Wrong rolled skill: the condition fails, so the grant does not apply.
        engine.roll_ctx.insert(
            "A".into(),
            crate::conditions::RollContext {
                skill: Some(Skill::Strike),
                gap: None,
                value: None,
                opp_skill: None,
            },
        );
        assert!(
            !engine.also_lead_now("A", &card_with),
            "rolled Strike, not Agility → grant does not apply"
        );
    }

    /// `RollWasSkill { who }` reads a specific side's turn roll (via the roll
    /// context's `opp_skill` for OPP). Under And it is Tomato Tomato Jr.'s Vine
    /// Time! "if BOTH players rolled Power, this card is also a Lead". schema v75
    #[test]
    fn also_lead_gates_on_both_players_turn_roll() {
        let vine_time: Card = serde_json::from_value(json!({
            "atk_type":"Submission","db_uuid":"vt","name":"Vine Time!","number":30,
            "play_order":"Finish","raw_text":"","tags":[],"finish_bonuses":{},
            "effects":[{"@type":"Effect","trigger":{"@type":"OnPlay"},"condition":{"@type":"Always"},
                "actions":[{"@type":"AlsoLead","order":"Lead","condition":{"@type":"And","items":[
                    {"@type":"RollWasSkill","skill":"Power","who":"SELF"},
                    {"@type":"RollWasSkill","skill":"Power","who":"OPP"}]}}],
                "duration":"INSTANT","optional":false,"raw_clause":"",
                "frequency":{"@type":"FrequencyGuard","kind":"UNLIMITED","n":null},"source":"card"}]
        }))
        .unwrap();
        let mut engine = engine();
        let ctx = |mine, theirs| crate::conditions::RollContext {
            skill: Some(mine),
            gap: None,
            value: None,
            opp_skill: Some(theirs),
        };

        // Only the owner rolled Power → the AND fails.
        engine
            .roll_ctx
            .insert("A".into(), ctx(Skill::Power, Skill::Strike));
        assert!(
            !engine.also_lead_now("A", &vine_time),
            "only SELF rolled Power → not both"
        );

        // Both sides rolled Power → the AND (SELF via skill, OPP via opp_skill) holds.
        engine
            .roll_ctx
            .insert("A".into(), ctx(Skill::Power, Skill::Power));
        assert!(
            engine.also_lead_now("A", &vine_time),
            "both players rolled Power → also a Lead"
        );
    }
}

#[cfg(test)]
mod bump_replacement_tests {
    use super::*;
    use crate::policy::{HeuristicPolicy, Policies};
    use serde_json::json;

    /// Pretty Paul "Let It Rip!": a Static once-per-match `BumpReplacement`, paired
    /// with an `OnBump` self-draw that must NOT fire when the bump is replaced.
    fn pretty_paul_gimmick() -> serde_json::Value {
        json!([
            {
                "@type": "Effect",
                "trigger": {"@type": "Static"},
                "condition": {"@type": "Always"},
                "actions": [{"@type": "BumpReplacement", "uses": 1, "draw": 2}],
                "duration": "WHILE_IN_PLAY",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "let it rip", "source": "entrance", "optional": false
            },
            {
                "@type": "Effect",
                "trigger": {"@type": "OnBump"},
                "condition": {"@type": "Always"},
                "actions": [{"@type": "Draw", "n": 1, "source": "TOP", "who": "SELF",
                    "cap": null, "per": null, "per_excludes_trigger": false, "per_who": "SELF"}],
                "duration": "INSTANT",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "on bump draw", "source": "gimmick", "optional": false
            }
        ])
    }

    fn filler(uuid: &str) -> Card {
        serde_json::from_value(
            json!({"atk_type": "Strike", "db_uuid": uuid, "name": "Filler",
            "number": 1, "play_order": "Lead", "raw_text": "", "tags": [], "finish_bonuses": {},
            "effects": []}),
        )
        .unwrap()
    }

    fn engine(a_gimmick: serde_json::Value) -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let deck = |id: &str, effects: serde_json::Value| -> Deck {
            serde_json::from_value(json!({
                "competitor": {"db_uuid": id, "name": id, "division": "World Championship",
                    "stats": stats, "effects": effects},
                "entrance": {"db_uuid": format!("{id}-ent"), "name": "ent"}, "cards": [],
            }))
            .expect("deck")
        };
        let pair = Policies::new(
            Box::new(HeuristicPolicy::heuristic()),
            Box::new(HeuristicPolicy::heuristic()),
        );
        Engine::new(
            deck("A", a_gimmick),
            deck("B", json!([])),
            Box::new(pair),
            1,
            String::new(),
            "sim".into(),
        )
    }

    #[test]
    fn replacement_fires_spends_charge_and_skips_the_bump_punish() {
        let mut e = engine(pretty_paul_gimmick());
        e.state.players.get_mut("A").unwrap().deck =
            (0..6).map(|i| filler(&format!("a{i}"))).collect();
        let hand_before = e.state.players["A"].hand.len();

        let out = e.try_bump_replacement(0).unwrap();
        assert!(
            out.is_some(),
            "the once-per-match replacement is offered and taken"
        );
        let (.., bumps) = out.unwrap();
        assert_eq!(bumps, 0, "a replaced bump is NOT counted as a bump");
        assert_eq!(
            e.state.players["A"].hand.len() - hand_before,
            2,
            "drew exactly `draw`=2 — the OnBump self-draw did NOT fire (bump was replaced)"
        );
        assert_eq!(
            e.state.players["A"].freq_counters.get("match:bump_replace"),
            Some(&1),
            "the per-match charge was spent"
        );

        // Charge spent: a second would-bump falls through to a normal bump.
        assert!(
            e.try_bump_replacement(0).unwrap().is_none(),
            "the once-per-match charge does not refill"
        );
    }

    #[test]
    fn no_replacement_without_the_declaration() {
        let mut e = engine(json!([]));
        assert!(e.bump_replacement_owner().is_none());
        assert!(e.try_bump_replacement(0).unwrap().is_none());
    }
}

#[cfg(test)]
mod reroll_cost_tests {
    use super::*;
    use crate::policy::{HeuristicPolicy, Policies};
    use serde_json::json;

    /// Mr. Hyde: a Static once-per-turn optional self re-roll costing an in-play
    /// "Potion" card shuffled into the deck.
    fn hyde_gimmick() -> serde_json::Value {
        json!([{
            "@type": "Effect",
            "trigger": {"@type": "Static"},
            "condition": {"@type": "Always"},
            "actions": [{"@type": "Reroll", "who": "SELF", "once": true, "choose": false,
                "when": "THIS", "cost": {"@type": "RerollCost", "kind": "SHUFFLE_IN_PLAY",
                    "count": null, "filter": {"@type": "CardFilter", "atk_type": null, "name": null,
                    "name_contains": ["Potion"], "number": null, "play_order": null,
                    "play_orders": [], "raw": null, "tag": null, "text_contains": []}}}],
            "duration": "INSTANT",
            "frequency": {"@type": "FrequencyGuard", "kind": "ONCE_PER_TURN", "n": null},
            "raw_clause": "hyde", "source": "gimmick", "optional": true
        }])
    }

    fn potion(uuid: &str) -> Card {
        serde_json::from_value(
            json!({"atk_type": "Strike", "db_uuid": uuid, "name": "Health Potion",
            "number": 1, "play_order": "Lead", "raw_text": "", "tags": [], "finish_bonuses": {},
            "effects": []}),
        )
        .unwrap()
    }

    fn engine() -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let deck = |id: &str, effects: serde_json::Value| -> Deck {
            serde_json::from_value(json!({
                "competitor": {"db_uuid": id, "name": id, "division": "World Championship",
                    "stats": stats, "effects": effects},
                "entrance": {"db_uuid": format!("{id}-ent"), "name": "ent"}, "cards": [],
            }))
            .expect("deck")
        };
        let pair = Policies::new(
            Box::new(HeuristicPolicy::heuristic()),
            Box::new(HeuristicPolicy::heuristic()),
        );
        Engine::new(
            deck("A", hyde_gimmick()),
            deck("B", json!([])),
            Box::new(pair),
            1,
            String::new(),
            "sim".into(),
        )
    }

    fn ctx() -> RollContext {
        RollContext {
            skill: Some(Skill::Power),
            gap: Some(0),
            value: Some(5),
            opp_skill: Some(Skill::Power),
        }
    }

    #[test]
    fn costed_reroll_fires_and_shuffles_the_potion_away() {
        let mut engine = engine();
        engine.state.players.get_mut("A").unwrap().in_play = vec![potion("p1")];
        let (own, opp) = (ctx(), ctx());
        let target = engine.offer_reroll("A", &own, &opp).unwrap();
        assert_eq!(
            target,
            Some("A".to_owned()),
            "the re-roll is offered and taken"
        );
        let a = &engine.state.players["A"];
        assert!(a.in_play.is_empty(), "the Potion left play");
        assert!(
            a.deck.iter().any(|c| c.db_uuid == "p1"),
            "the Potion was shuffled into the deck"
        );
    }

    #[test]
    fn costed_reroll_is_not_offered_without_the_potion() {
        let mut engine = engine();
        // A has an in-play card that is NOT a Potion — cannot pay.
        engine.state.players.get_mut("A").unwrap().in_play = vec![serde_json::from_value(
            json!({"atk_type": "Strike", "db_uuid": "x", "name": "Chair",
                "number": 1, "play_order": "Lead", "raw_text": "", "tags": [],
                "finish_bonuses": {}, "effects": []}),
        )
        .unwrap()];
        let (own, opp) = (ctx(), ctx());
        assert_eq!(engine.offer_reroll("A", &own, &opp).unwrap(), None);
        // The non-Potion card is untouched.
        assert_eq!(engine.state.players["A"].in_play.len(), 1);
    }
}

#[cfg(test)]
mod on_rolled_all_tests {
    use super::*;
    use crate::policy::{HeuristicPolicy, Policies};
    use serde_json::json;

    /// General Lee Wong V2: OnRolledAll{P,A,T} -> Draw 3 + next roll +2.
    fn glw_gimmick() -> serde_json::Value {
        json!([{
            "@type": "Effect",
            "trigger": {"@type": "OnRolledAll", "skills": ["Power", "Agility", "Technique"], "who": "SELF"},
            "condition": {"@type": "Always"},
            "actions": [
                {"@type": "Draw", "n": 3, "source": "TOP", "who": "SELF", "cap": null,
                 "per": null, "per_excludes_trigger": false, "per_who": "SELF"},
                {"@type": "ModifyRoll", "who": "SELF", "delta": 2, "when": "NEXT",
                 "per": null, "per_who": "SELF"}
            ],
            "duration": "INSTANT",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "glw", "source": "gimmick", "optional": false
        }])
    }

    fn card(uuid: &str) -> serde_json::Value {
        json!({"atk_type": "Strike", "db_uuid": uuid, "name": uuid, "number": 1,
               "play_order": "Lead", "raw_text": "", "tags": [], "finish_bonuses": {}, "effects": []})
    }

    fn engine() -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let deck_a: Deck = serde_json::from_value(json!({
            "competitor": {"db_uuid": "A", "name": "A", "division": "World Championship",
                "stats": stats, "effects": glw_gimmick()},
            "entrance": {"db_uuid": "A-ent", "name": "ent"},
            "cards": (0..3).map(|i| card(&format!("c{i}"))).collect::<Vec<_>>(),
        }))
        .expect("deck A");
        let deck_b: Deck = serde_json::from_value(json!({
            "competitor": {"db_uuid": "B", "name": "B", "division": "World Championship",
                "stats": stats, "effects": []},
            "entrance": {"db_uuid": "B-ent", "name": "ent"}, "cards": [],
        }))
        .expect("deck B");
        let pair = Policies::new(
            Box::new(HeuristicPolicy::heuristic()),
            Box::new(HeuristicPolicy::heuristic()),
        );
        Engine::new(
            deck_a,
            deck_b,
            Box::new(pair),
            1,
            String::new(),
            "sim".into(),
        )
    }

    fn roll(engine: &mut Engine, skill: Skill) {
        engine.roll_ctx.insert(
            "A".to_owned(),
            RollContext {
                skill: Some(skill),
                gap: Some(0),
                value: Some(5),
                opp_skill: Some(Skill::Power),
            },
        );
        engine.run_on_rolled_all("A").unwrap();
    }

    #[test]
    fn fires_only_after_all_three_distinct_skills_then_resets() {
        let mut engine = engine();
        engine.state.players.get_mut("A").unwrap().deck = (0..6)
            .map(|i| serde_json::from_value(card(&format!("d{i}"))).unwrap())
            .collect();
        // Power then Agility: incomplete, no reward.
        roll(&mut engine, Skill::Power);
        roll(&mut engine, Skill::Agility);
        assert_eq!(engine.state.players["A"].hand.len(), 0);
        // Repeating Power is idempotent — the set stays {P, A}.
        roll(&mut engine, Skill::Power);
        assert_eq!(engine.state.players["A"].hand.len(), 0);
        // Technique completes {P, A, T}: draw 3 + next roll +2, and the set resets.
        roll(&mut engine, Skill::Technique);
        assert_eq!(
            engine.state.players["A"].hand.len(),
            3,
            "drew 3 on completion"
        );
        assert_eq!(
            engine.state.players["A"].pending_roll_mods.next_turn, 2,
            "next roll +2"
        );
        // The accumulator reset — one more Technique does NOT re-fire.
        roll(&mut engine, Skill::Technique);
        assert_eq!(
            engine.state.players["A"].hand.len(),
            3,
            "no re-fire until the set is rebuilt"
        );
    }

    #[test]
    fn a_non_required_skill_does_not_accumulate() {
        let mut engine = engine();
        engine.state.players.get_mut("A").unwrap().deck = (0..6)
            .map(|i| serde_json::from_value(card(&format!("d{i}"))).unwrap())
            .collect();
        // Submission/Grapple/Strike are not in the set — no progress toward the reward.
        roll(&mut engine, Skill::Submission);
        roll(&mut engine, Skill::Grapple);
        roll(&mut engine, Skill::Power);
        roll(&mut engine, Skill::Agility);
        roll(&mut engine, Skill::Technique);
        assert_eq!(
            engine.state.players["A"].hand.len(),
            3,
            "only P/A/T count toward completion"
        );
    }
}

/// A grammar-produced `OnRoll -> Draw` on a main-deck card fires while that card is
/// IN PLAY (task #49) — proving the parser may safely emit OnRoll for in-play cards,
/// not just gimmick/entrance overrides.
#[cfg(test)]
mod on_roll_in_play_draw_tests {
    use super::*;
    use crate::policy::{HeuristicPolicy, Policies};
    use serde_json::json;

    fn draw_on_roll(skill: &str, who: &str) -> serde_json::Value {
        json!([{
            "@type": "Effect",
            "trigger": {"@type": "OnRoll", "skill": skill, "who": who},
            "condition": {"@type": "Always"},
            "actions": [{"@type": "Draw", "n": 1, "source": "TOP", "who": "SELF",
                "cap": null, "per": null, "per_excludes_trigger": false, "per_who": "SELF"}],
            "duration": "INSTANT",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "when roll -> draw", "source": "card", "optional": false
        }])
    }

    fn card_with(uuid: &str, effects: serde_json::Value) -> serde_json::Value {
        json!({"atk_type": "Strike", "db_uuid": uuid, "name": uuid, "number": 1,
               "play_order": "Lead", "raw_text": "", "tags": [], "finish_bonuses": {},
               "effects": effects})
    }

    fn engine() -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let mk = |k: &str| {
            serde_json::from_value::<Deck>(json!({
                "competitor": {"db_uuid": k, "name": k, "division": "World Championship",
                    "stats": stats, "effects": []},
                "entrance": {"db_uuid": format!("{k}-ent"), "name": "ent"}, "cards": [],
            }))
            .expect("deck")
        };
        let pair = Policies::new(
            Box::new(HeuristicPolicy::heuristic()),
            Box::new(HeuristicPolicy::heuristic()),
        );
        Engine::new(
            mk("A"),
            mk("B"),
            Box::new(pair),
            1,
            String::new(),
            "sim".into(),
        )
    }

    fn set_roll(engine: &mut Engine, key: &str, skill: Skill) {
        engine.roll_ctx.insert(
            key.to_owned(),
            RollContext {
                skill: Some(skill),
                gap: Some(0),
                value: Some(5),
                opp_skill: None,
            },
        );
    }

    #[test]
    fn self_roll_in_play_card_draws() {
        let mut engine = engine();
        // A card in A's play zone with "when you roll Technique, draw 1".
        let card = serde_json::from_value(card_with("t", draw_on_roll("Technique", "SELF")))
            .expect("card");
        engine
            .state
            .players
            .get_mut("A")
            .unwrap()
            .in_play
            .push(card);
        engine.state.players.get_mut("A").unwrap().deck = (0..3)
            .map(|i| serde_json::from_value(card_with(&format!("d{i}"), json!([]))).unwrap())
            .collect();
        // Wrong skill: no draw.
        set_roll(&mut engine, "A", Skill::Power);
        engine.run_on_roll("A").unwrap();
        assert_eq!(engine.state.players["A"].hand.len(), 0, "no draw off Power");
        // Matching skill: draw 1.
        set_roll(&mut engine, "A", Skill::Technique);
        engine.run_on_roll("A").unwrap();
        assert_eq!(engine.state.players["A"].hand.len(), 1, "drew on Technique");
    }

    #[test]
    fn opponent_roll_triggers_owner_draw() {
        let mut engine = engine();
        // A's card: "when your OPPONENT rolls Power, draw 1" (who=OPP).
        let card =
            serde_json::from_value(card_with("p", draw_on_roll("Power", "OPP"))).expect("card");
        engine
            .state
            .players
            .get_mut("A")
            .unwrap()
            .in_play
            .push(card);
        engine.state.players.get_mut("A").unwrap().deck = (0..2)
            .map(|i| serde_json::from_value(card_with(&format!("d{i}"), json!([]))).unwrap())
            .collect();
        // B (A's opponent) rolls Power -> A draws. run_on_roll fires for each roller.
        set_roll(&mut engine, "B", Skill::Power);
        engine.run_on_roll("A").unwrap();
        assert_eq!(
            engine.state.players["A"].hand.len(),
            1,
            "A drew on opp Power roll"
        );
    }
}

/// Base-roll-gated Finish bonus (task #49): "If your Finish roll is N or less/greater,
/// it is +M" gates on the BASE roll (skill stat, pre-bonus). Exercises the protected
/// `finish_roll_bonus` directly.
#[cfg(test)]
mod finish_base_gate_tests {
    use super::*;
    use crate::policy::{HeuristicPolicy, Policies};
    use serde_json::json;

    fn frb(delta: i64, le: Option<i64>, ge: Option<i64>) -> serde_json::Value {
        json!([{
            "@type": "Effect", "trigger": {"@type": "Static"},
            "condition": {"@type": "Always"},
            "actions": [{"@type": "FinishRollBonus", "delta": delta, "when_skill": null,
                "either": false, "when_base_le": le, "when_base_ge": ge, "per": null,
                "per_who": "SELF", "per_zone": "IN_PLAY"}],
            "duration": "WHILE_IN_PLAY",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "", "source": "card", "optional": false
        }])
    }

    fn engine_with(effects: serde_json::Value) -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let mk = |k: &str, eff: serde_json::Value| {
            serde_json::from_value::<Deck>(json!({
                "competitor": {"db_uuid": k, "name": k, "division": "World Championship",
                    "stats": stats, "effects": []},
                "entrance": {"db_uuid": format!("{k}-ent"), "name": "ent"},
                "cards": [{"atk_type": "Strike", "db_uuid": "fc", "name": "fc", "number": 1,
                    "play_order": "Finish", "raw_text": "", "tags": [], "finish_bonuses": {},
                    "effects": eff}],
            }))
            .expect("deck")
        };
        let pair = Policies::new(
            Box::new(HeuristicPolicy::heuristic()),
            Box::new(HeuristicPolicy::heuristic()),
        );
        let mut engine = Engine::new(
            mk("A", effects),
            mk("B", json!([])),
            Box::new(pair),
            1,
            String::new(),
            "sim".into(),
        );
        // Move A's Finish card (carrying the bonus) into play.
        let card = engine.state.players.get_mut("A").unwrap().deck.remove(0);
        engine
            .state
            .players
            .get_mut("A")
            .unwrap()
            .in_play
            .push(card);
        engine
    }

    #[test]
    fn or_less_applies_only_at_or_below_threshold() {
        let engine = engine_with(frb(2, Some(6), None));
        // base 5 (<= 6): +2; base 6: +2; base 7 (> 6): 0.
        assert_eq!(engine.finish_roll_bonus("A", Skill::Power, 5), 2);
        assert_eq!(engine.finish_roll_bonus("A", Skill::Power, 6), 2);
        assert_eq!(engine.finish_roll_bonus("A", Skill::Power, 7), 0);
    }

    #[test]
    fn or_greater_applies_only_at_or_above_threshold_and_signed() {
        // "If your Finish roll is 8 or greater, it is -3" (a negative rider).
        let engine = engine_with(frb(-3, None, Some(8)));
        assert_eq!(engine.finish_roll_bonus("A", Skill::Power, 8), -3);
        assert_eq!(engine.finish_roll_bonus("A", Skill::Power, 9), -3);
        assert_eq!(engine.finish_roll_bonus("A", Skill::Power, 7), 0);
    }

    /// Per-count Finish bonus (task #131, v106): `+delta` per matching card, with a
    /// `cap` clamp and `per_excludes_self` dropping the SOURCE card (the "fc" Finish, a
    /// Strike, from `engine_with`). Exercises the refactored source-threaded fold.
    fn frb_per(delta: i64, atk: &str, exclude: bool, cap: Option<i64>) -> serde_json::Value {
        json!([{
            "@type": "Effect", "trigger": {"@type": "Static"},
            "condition": {"@type": "Always"},
            "actions": [{"@type": "FinishRollBonus", "delta": delta, "when_skill": null,
                "either": false, "when_base_le": null, "when_base_ge": null,
                "per": {"@type": "CardFilter", "atk_type": atk}, "per_who": "SELF",
                "per_zone": "IN_PLAY", "cap": cap, "per_excludes_self": exclude}],
            "duration": "WHILE_IN_PLAY",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "", "source": "card", "optional": false
        }])
    }

    fn strike(uuid: &str) -> Card {
        serde_json::from_value(json!({"atk_type": "Strike", "db_uuid": uuid, "name": uuid,
            "number": 1, "play_order": "Lead", "raw_text": "", "tags": [], "finish_bonuses": {},
            "effects": []}))
        .unwrap()
    }

    #[test]
    fn per_count_finish_bonus_excludes_source_and_caps() {
        let add_strikes = |e: &mut Engine, n: usize| {
            let ip = &mut e.state.players.get_mut("A").unwrap().in_play;
            for i in 0..n {
                ip.push(strike(&format!("x{i}")));
            }
        };

        // "+1 for each OTHER Strike you have in play": source fc + 1 extra -> counts 1.
        let mut engine = engine_with(frb_per(1, "Strike", true, None));
        add_strikes(&mut engine, 1);
        assert_eq!(
            engine.finish_roll_bonus("A", Skill::Power, 5),
            1,
            "source excluded"
        );

        // No exclude: fc + 1 extra both count -> +2 (the control).
        let mut engine = engine_with(frb_per(1, "Strike", false, None));
        add_strikes(&mut engine, 1);
        assert_eq!(
            engine.finish_roll_bonus("A", Skill::Power, 5),
            2,
            "source counts itself"
        );

        // "(Max +2)" clamps the product: fc + 2 extra = 3 Strikes, +1 each, cap 2 -> 2.
        let mut engine = engine_with(frb_per(1, "Strike", false, Some(2)));
        add_strikes(&mut engine, 2);
        assert_eq!(
            engine.finish_roll_bonus("A", Skill::Power, 5),
            2,
            "clamped to cap"
        );
    }
}

/// Symmetric "if either player rolls <S> for their {turn|breakout|Finish} roll" modifier
/// (task #131, v107): an `either` mod on ONE player's board reaches the OTHER player's
/// roll (it applies to whoever rolls), while a non-`either` mod stays with its owner.
#[cfg(test)]
mod either_roll_tests {
    use super::*;
    use crate::policy::{HeuristicPolicy, Policies};
    use serde_json::json;

    fn eng() -> Engine {
        let deck = |id: &str| -> Deck {
            serde_json::from_value(json!({
                "competitor": {"db_uuid": id, "name": id, "division": "World Championship",
                    "stats": {"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5},
                    "effects": []},
                "entrance": {"db_uuid": format!("{id}-ent"), "name": "ent"}, "cards": [],
            }))
            .expect("deck")
        };
        Engine::new(
            deck("A"),
            deck("B"),
            Box::new(Policies::new(
                Box::new(HeuristicPolicy::heuristic()),
                Box::new(HeuristicPolicy::heuristic()),
            )),
            1,
            String::new(),
            "sim".into(),
        )
    }

    fn card_with(action: serde_json::Value) -> Card {
        serde_json::from_value(json!({"atk_type": "Strike", "db_uuid": "e", "name": "e",
            "number": 1, "play_order": "Lead", "raw_text": "", "tags": [], "finish_bonuses": {},
            "effects": [{"@type": "Effect", "trigger": {"@type": "Static"},
                "condition": {"@type": "Always"}, "duration": "WHILE_IN_PLAY",
                "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
                "raw_clause": "", "source": "card", "optional": false, "actions": [action]}]}))
        .unwrap()
    }

    fn put_on_b(e: &mut Engine, action: serde_json::Value) {
        e.state.players.get_mut("B").unwrap().in_play = vec![card_with(action)];
    }

    #[test]
    fn either_modifier_reaches_the_opponent_of_its_owner() {
        // Turn: "if either player rolls Power for their turn roll, +1" on B's board.
        let mut e = eng();
        put_on_b(
            &mut e,
            json!({"@type": "TurnRollBonus", "skill": "Power", "delta": 1, "either": true}),
        );
        assert_eq!(
            e.turn_roll_bonus("A", Skill::Power),
            1,
            "A gets B's either turn mod"
        );
        assert_eq!(e.turn_roll_bonus("A", Skill::Agility), 0, "skill-gated");
        assert_eq!(e.turn_roll_bonus("B", Skill::Power), 1, "owner gets it too");

        // Breakout: "if either player rolls Agility for their breakout roll, -1" on B.
        let mut e = eng();
        put_on_b(
            &mut e,
            json!({"@type": "BreakoutModifier", "delta": -1, "attempts": null,
            "when_skill": "Agility", "who": "SELF", "either": true}),
        );
        assert_eq!(
            e.breakout_bonus("A", 1, Skill::Agility),
            -1,
            "A's Agility breakout gets it"
        );
        assert_eq!(e.breakout_bonus("A", 1, Skill::Power), 0, "skill-gated");

        // Finish: "if either player rolls Submission for their Finish roll, -1" on B —
        // exercises the previously-dead `either` field, now read from the other board.
        let mut e = eng();
        put_on_b(
            &mut e,
            json!({"@type": "FinishRollBonus", "delta": -1, "when_skill": "Submission",
            "either": true, "when_base_le": null, "when_base_ge": null, "per": null,
            "per_who": "SELF", "per_zone": "IN_PLAY"}),
        );
        assert_eq!(
            e.finish_roll_bonus("A", Skill::Submission, 5),
            -1,
            "A's Submission finish gets it"
        );
        assert_eq!(e.finish_roll_bonus("A", Skill::Power, 5), 0, "skill-gated");
    }

    #[test]
    fn non_either_modifier_stays_with_its_owner() {
        let mut e = eng();
        put_on_b(
            &mut e,
            json!({"@type": "TurnRollBonus", "skill": "Power", "delta": 1, "either": false}),
        );
        assert_eq!(
            e.turn_roll_bonus("A", Skill::Power),
            0,
            "self-only mod does not cross boards"
        );
        assert_eq!(
            e.turn_roll_bonus("B", Skill::Power),
            1,
            "owner still gets it"
        );
    }
}

/// Crowd-Meter count draw (task #131, v108): a `Draw{from_crowd}` draws Crowd Meter + `n`
/// (the signed offset), clamped to `cap` and floored at 0.
#[cfg(test)]
mod crowd_draw_tests {
    use super::*;
    use crate::policy::{HeuristicPolicy, Policies};
    use serde_json::json;

    fn eng() -> Engine {
        let deck = |id: &str| -> Deck {
            serde_json::from_value(json!({
                "competitor": {"db_uuid": id, "name": id, "division": "World Championship",
                    "stats": {"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5},
                    "effects": []},
                "entrance": {"db_uuid": format!("{id}-ent"), "name": "ent"}, "cards": [],
            }))
            .expect("deck")
        };
        Engine::new(
            deck("A"),
            deck("B"),
            Box::new(Policies::new(
                Box::new(HeuristicPolicy::heuristic()),
                Box::new(HeuristicPolicy::heuristic()),
            )),
            1,
            String::new(),
            "sim".into(),
        )
    }

    fn crowd_draw(n: i64, cap: Option<i64>) -> DrawSpec {
        DrawSpec {
            n,
            source: DeckEnd::Top,
            who: Who::SelfSide,
            per: None,
            per_who: Who::SelfSide,
            cap,
            per_excludes_trigger: false,
            from_crowd: true,
        }
    }

    /// Draw count = Crowd Meter + offset, capped; measured on the deck (unaffected by the
    /// hand cap, which could trim the hand after the draw).
    fn drew(cm: i64, spec_n: i64, cap: Option<i64>) -> i64 {
        let mut e = eng();
        e.state.crowd_meter = cm;
        let deck: Vec<Card> = (0..20)
            .map(|i| {
                serde_json::from_value(json!({"atk_type": "Strike", "db_uuid": format!("d{i}"),
                    "name": "d", "number": 1, "play_order": "Lead", "raw_text": "",
                    "tags": [], "finish_bonuses": {}, "effects": []}))
                .unwrap()
            })
            .collect();
        let before = deck.len();
        e.state.players.get_mut("A").unwrap().deck = deck;
        e.act_draw(crowd_draw(spec_n, cap), "A").unwrap();
        (before - e.state.players["A"].deck.len()) as i64
    }

    #[test]
    fn draw_equals_crowd_meter_plus_offset_capped_and_floored() {
        assert_eq!(drew(3, 0, None), 3, "equal to the Crowd Meter");
        assert_eq!(drew(3, 1, None), 4, "Crowd Meter +1");
        assert_eq!(drew(3, 1, Some(2)), 2, "clamped to (Max +2)");
        assert_eq!(drew(0, -1, None), 0, "floored at 0, never negative");
    }
}

#[cfg(test)]
mod mack_a_tack_tests {
    use super::*;
    use crate::policy::{HeuristicPolicy, Policies};
    use serde_json::json;

    fn card(uuid: &str) -> Card {
        serde_json::from_value(json!({"atk_type": "Strike", "db_uuid": uuid, "name": uuid,
            "number": 1, "play_order": "Lead", "raw_text": "", "tags": [], "finish_bonuses": {},
            "effects": []}))
        .unwrap()
    }

    fn engine(a_effects: serde_json::Value) -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let deck = |id: &str, effects: serde_json::Value| -> Deck {
            serde_json::from_value(json!({
                "competitor": {"db_uuid": id, "name": id, "division": "World Championship",
                    "stats": stats, "effects": effects},
                "entrance": {"db_uuid": format!("{id}-ent"), "name": "ent"}, "cards": [],
            }))
            .expect("deck")
        };
        let pair = Policies::new(
            Box::new(HeuristicPolicy::heuristic()),
            Box::new(HeuristicPolicy::heuristic()),
        );
        Engine::new(
            deck("A", a_effects),
            deck("B", json!([])),
            Box::new(pair),
            1,
            String::new(),
            "sim".into(),
        )
    }

    fn bump_replace() -> serde_json::Value {
        json!([{
            "@type": "Effect", "trigger": {"@type": "Static"}, "condition": {"@type": "Always"},
            "actions": [{"@type": "BumpDrawReplace"}], "duration": "WHILE_IN_PLAY",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "m", "source": "gimmick", "optional": false
        }])
    }

    #[test]
    fn bump_replace_makes_the_declarers_opponent_discard_instead_of_drawing() {
        let mut engine = engine(bump_replace());
        // B (the declarer A's opponent) has a hand to discard and a deck it would draw from.
        engine.state.players.get_mut("B").unwrap().hand = vec![card("b1"), card("b2")];
        engine.state.players.get_mut("B").unwrap().deck = vec![card("bd")];
        engine.state.players.get_mut("A").unwrap().deck = vec![card("ad")];
        // B's bump card: A declares BumpDrawReplace -> B discards 1 instead of drawing.
        engine.bump_draw("B").unwrap();
        assert_eq!(engine.state.players["B"].hand.len(), 1, "B discarded 1");
        assert_eq!(
            engine.state.players["B"].deck.len(),
            1,
            "B did NOT draw (deck unchanged)"
        );
        // A's bump card: B declares nothing -> A draws normally.
        engine.bump_draw("A").unwrap();
        assert_eq!(engine.state.players["A"].hand.len(), 1, "A drew 1");
        assert_eq!(engine.state.players["A"].deck.len(), 0);
    }

    #[test]
    fn bumped_last_turn_roll_condition_reads_the_state_flag() {
        let engine = engine(json!([]));
        let cond: Condition =
            serde_json::from_value(json!({"@type": "BumpedLastTurnRoll"})).unwrap();
        // Default false, then true once the flag is set.
        assert!(!conditions::holds(&cond, &engine.state, "A", None));
        let mut engine = engine;
        engine.state.last_turn_bumped = true;
        assert!(conditions::holds(&cond, &engine.state, "A", None));
    }
}

#[cfg(test)]
mod candyman_dan_tests {
    use super::*;
    use crate::policy::{HeuristicPolicy, Policies};
    use serde_json::json;

    fn card(uuid: &str, order: &str) -> Card {
        serde_json::from_value(json!({"atk_type": "Strike", "db_uuid": uuid, "name": uuid,
            "number": 1, "play_order": order, "raw_text": "", "tags": [], "finish_bonuses": {},
            "effects": []}))
        .unwrap()
    }

    fn engine() -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let deck = |id: &str| -> Deck {
            serde_json::from_value(json!({
                "competitor": {"db_uuid": id, "name": id, "division": "World Championship",
                    "stats": stats, "effects": []},
                "entrance": {"db_uuid": format!("{id}-ent"), "name": "ent"}, "cards": [],
            }))
            .expect("deck")
        };
        let pair = Policies::new(
            Box::new(HeuristicPolicy::heuristic()),
            Box::new(HeuristicPolicy::heuristic()),
        );
        Engine::new(
            deck("A"),
            deck("B"),
            Box::new(pair),
            1,
            String::new(),
            "sim".into(),
        )
    }

    #[test]
    fn discards_own_then_the_opponents_same_order_card() {
        let mut engine = engine();
        engine.state.players.get_mut("A").unwrap().in_play = vec![card("a-fu", "Followup")];
        engine.state.players.get_mut("B").unwrap().in_play =
            vec![card("b-fu", "Followup"), card("b-lead", "Lead")];
        engine.act_discard_in_play_match("A").unwrap();
        // A discarded its own Follow Up; B lost its Follow Up (same order), kept its Lead.
        assert!(engine.state.players["A"].in_play.is_empty());
        assert!(engine.state.players["A"]
            .discard
            .iter()
            .any(|c| c.db_uuid == "a-fu"));
        assert!(engine.state.players["B"]
            .in_play
            .iter()
            .any(|c| c.db_uuid == "b-lead"));
        assert!(!engine.state.players["B"]
            .in_play
            .iter()
            .any(|c| c.db_uuid == "b-fu"));
        assert!(engine.state.players["B"]
            .discard
            .iter()
            .any(|c| c.db_uuid == "b-fu"));
    }

    #[test]
    fn no_matching_opponent_card_discards_only_your_own() {
        let mut engine = engine();
        engine.state.players.get_mut("A").unwrap().in_play = vec![card("a-fin", "Finish")];
        engine.state.players.get_mut("B").unwrap().in_play = vec![card("b-lead", "Lead")];
        engine.act_discard_in_play_match("A").unwrap();
        // A discarded its Finish; B has no Finish, so its Lead is untouched.
        assert!(engine.state.players["A"].in_play.is_empty());
        assert_eq!(engine.state.players["B"].in_play.len(), 1);
    }

    #[test]
    fn no_own_in_play_is_a_noop() {
        let mut engine = engine();
        engine.state.players.get_mut("B").unwrap().in_play = vec![card("b-fu", "Followup")];
        engine.act_discard_in_play_match("A").unwrap();
        assert_eq!(
            engine.state.players["B"].in_play.len(),
            1,
            "nothing to trade -> B untouched"
        );
    }
}

#[cfg(test)]
mod memes_dealer_tests {
    use super::*;
    use crate::policy::{HeuristicPolicy, Policies};
    use serde_json::{json, Value};

    fn card(uuid: &str) -> Card {
        serde_json::from_value(json!({"atk_type": "Strike", "db_uuid": uuid, "name": uuid,
            "number": 1, "play_order": "Lead", "raw_text": "", "tags": [], "finish_bonuses": {},
            "effects": []}))
        .unwrap()
    }

    fn engine(a_effects: Value) -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let deck = |id: &str, effects: Value| -> Deck {
            serde_json::from_value(json!({
                "competitor": {"db_uuid": id, "name": id, "division": "World Championship",
                    "stats": stats, "effects": effects},
                "entrance": {"db_uuid": format!("{id}-ent"), "name": "ent"}, "cards": [],
            }))
            .expect("deck")
        };
        let pair = Policies::new(
            Box::new(HeuristicPolicy::heuristic()),
            Box::new(HeuristicPolicy::heuristic()),
        );
        Engine::new(
            deck("A", a_effects),
            deck("B", json!([])),
            Box::new(pair),
            1,
            String::new(),
            "sim".into(),
        )
    }

    #[test]
    fn reveal_one_shuffles_a_single_card_and_draws() {
        let mut engine = engine(json!([]));
        engine.state.players.get_mut("A").unwrap().hand = vec![card("h1"), card("h2")];
        engine.state.players.get_mut("A").unwrap().deck = vec![card("d1"), card("d2")];
        // Reveal+shuffle 1 chosen card, draw 1: hand size unchanged, one hand card left play.
        engine
            .act_shuffle_hand_draw(Who::SelfSide, 1, false, Some(1), "A")
            .unwrap();
        let a = &engine.state.players["A"];
        // hand size 2 proves exactly 1 was shed (a whole-hand shuffle would leave 1).
        assert_eq!(a.hand.len(), 2, "shed 1 + drew 1 -> net unchanged");
        assert_eq!(a.hand.len() + a.deck.len(), 4, "no cards lost");
    }

    #[test]
    fn whole_hand_path_is_unchanged_when_hand_count_is_none() {
        let mut engine = engine(json!([]));
        engine.state.players.get_mut("A").unwrap().hand = vec![card("h1"), card("h2"), card("h3")];
        engine.state.players.get_mut("A").unwrap().deck = vec![card("d1")];
        engine
            .act_shuffle_hand_draw(Who::SelfSide, 2, false, None, "A")
            .unwrap();
        let a = &engine.state.players["A"];
        assert_eq!(a.hand.len(), 2, "whole hand shuffled, drew 2");
        assert_eq!(a.hand.len() + a.deck.len(), 4);
    }

    #[test]
    fn during_opponent_turn_fires_for_the_non_active_player() {
        let gimmick = json!([{
            "@type": "Effect", "trigger": {"@type": "DuringOpponentTurn"},
            "condition": {"@type": "Always"},
            "actions": [{"@type": "Draw", "n": 1, "source": "TOP", "who": "SELF", "cap": null,
                "per": null, "per_excludes_trigger": false, "per_who": "SELF"}],
            "duration": "INSTANT",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "m", "source": "gimmick", "optional": false
        }]);
        let mut engine = engine(gimmick);
        engine.state.players.get_mut("A").unwrap().deck = vec![card("d1")];
        engine.run_opponent_turn("A").unwrap();
        assert_eq!(
            engine.state.players["A"].hand.len(),
            1,
            "A acted during the opponent's turn"
        );
    }
}

#[cfg(test)]
mod pedro_valiant_tests {
    use super::*;
    use crate::policy::{HeuristicPolicy, Policies};
    use serde_json::{json, Value};

    /// An entrance whose effects are a Static [Draw 1, ModifyRoll +1] — a stand-in for
    /// a modeled "Training with" entrance (the real ones parse to Unsupported).
    fn entrance(name: &str) -> Value {
        json!({"db_uuid": "ent", "name": name, "effects": [{
            "@type": "Effect", "trigger": {"@type": "Static"}, "condition": {"@type": "Always"},
            "actions": [
                {"@type": "Draw", "n": 1, "source": "TOP", "who": "SELF", "cap": null,
                 "per": null, "per_excludes_trigger": false, "per_who": "SELF"},
                {"@type": "ModifyRoll", "who": "SELF", "delta": 1, "when": "NEXT",
                 "per": null, "per_who": "SELF"}
            ],
            "duration": "INSTANT",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "e", "source": "entrance", "optional": false
        }]})
    }

    fn scale_decl() -> Value {
        json!({
            "@type": "Effect", "trigger": {"@type": "Static"}, "condition": {"@type": "Always"},
            "actions": [{"@type": "ScaleEntranceNumbers", "name_contains": ["Training with"],
                         "factor": 3}],
            "duration": "WHILE_IN_PLAY",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "p", "source": "gimmick", "optional": false
        })
    }

    fn engine(a_effects: Value, ent_name: &str) -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let deck_a: Deck = serde_json::from_value(json!({
            "competitor": {"db_uuid": "A", "name": "A", "division": "World Championship",
                "stats": stats, "effects": a_effects},
            "entrance": entrance(ent_name), "cards": [],
        }))
        .expect("deck A");
        let deck_b: Deck = serde_json::from_value(json!({
            "competitor": {"db_uuid": "B", "name": "B", "division": "World Championship",
                "stats": stats, "effects": []},
            "entrance": {"db_uuid": "B-ent", "name": "ent"}, "cards": [],
        }))
        .expect("deck B");
        let pair = Policies::new(
            Box::new(HeuristicPolicy::heuristic()),
            Box::new(HeuristicPolicy::heuristic()),
        );
        Engine::new(
            deck_a,
            deck_b,
            Box::new(pair),
            1,
            String::new(),
            "sim".into(),
        )
    }

    /// The entrance's Draw.n and ModifyRoll.delta as seen in standing_effects.
    fn entrance_numbers(engine: &Engine) -> (i64, i64) {
        let (mut n, mut d) = (0, 0);
        for eff in engine.standing_effects("A") {
            for a in &eff.actions {
                match a {
                    Action::Draw { n: dn, .. } => n = *dn,
                    Action::ModifyRoll { delta, .. } => d = *delta,
                    _ => {}
                }
            }
        }
        (n, d)
    }

    #[test]
    fn scales_a_matching_entrances_numbers() {
        let engine = engine(json!([scale_decl()]), "Power Training with Rock Newman");
        assert_eq!(
            entrance_numbers(&engine),
            (3, 3),
            "draw 1 -> 3, +1 roll -> +3"
        );
    }

    #[test]
    fn does_not_scale_a_non_matching_entrance() {
        // Entrance name lacks "Training with" -> numbers untouched.
        let engine = engine(json!([scale_decl()]), "Some Other Entrance");
        assert_eq!(entrance_numbers(&engine), (1, 1));
    }

    #[test]
    fn a_blanked_gimmick_stops_scaling() {
        let mut engine = engine(json!([scale_decl()]), "Power Training with Rock Newman");
        assert_eq!(entrance_numbers(&engine), (3, 3));
        engine.state.players.get_mut("A").unwrap().gimmick_blanked = true;
        assert_eq!(
            entrance_numbers(&engine),
            (1, 1),
            "blanked gimmick declares nothing"
        );
    }
}

#[cfg(test)]
mod el_super_hombre_v3_tests {
    use super::*;
    use serde_json::{json, Value};

    /// Picks the choice option with the given index at a "choice" point; legal[0] else.
    struct PickChoice(usize);
    impl Decider for PickChoice {
        fn decide(
            &mut self,
            point: &str,
            _: &str,
            legal: &[Value],
            _: &mut GameState,
        ) -> Option<Value> {
            if point == "choice" {
                legal
                    .iter()
                    .find(|o| o["index"].as_u64() == Some(self.0 as u64))
                    .cloned()
            } else {
                legal.first().cloned()
            }
        }
        fn policy_name(&self, _: &str) -> String {
            "pick".to_owned()
        }
    }

    /// El Super Hombre V3: OnRollBoost(Agility) -> Choice[Draw 1 | RollBoost +1].
    fn gimmick() -> Value {
        json!([{
            "@type": "Effect",
            "trigger": {"@type": "OnRollBoost", "skill": "Agility", "delta": 0, "on_bump": false},
            "condition": {"@type": "Always"},
            "actions": [{"@type": "Choice", "options": [
                {"@type": "ChoiceOption", "label": "draw", "actions": [{"@type": "Draw", "n": 1,
                    "source": "TOP", "who": "SELF", "cap": null, "per": null,
                    "per_excludes_trigger": false, "per_who": "SELF"}]},
                {"@type": "ChoiceOption", "label": "boost", "actions": [{"@type": "RollBoost", "delta": 1}]}
            ]}],
            "duration": "INSTANT",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "eshv3", "source": "gimmick", "optional": false
        }])
    }

    fn engine(pick: usize) -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let deck = |id: &str, effects: Value| -> Deck {
            serde_json::from_value(json!({
                "competitor": {"db_uuid": id, "name": id, "division": "World Championship",
                    "stats": stats, "effects": effects},
                "entrance": {"db_uuid": format!("{id}-ent"), "name": "ent"},
                "cards": [{"atk_type": "Strike", "db_uuid": "c", "name": "c", "number": 1,
                    "play_order": "Lead", "raw_text": "", "tags": [], "finish_bonuses": {}, "effects": []}],
            }))
            .expect("deck")
        };
        Engine::new(
            deck("A", gimmick()),
            deck("B", json!([])),
            Box::new(PickChoice(pick)),
            1,
            String::new(),
            "sim".into(),
        )
    }

    #[test]
    fn boost_branch_adds_one_to_the_current_roll() {
        let mut engine = engine(1); // pick "boost"
        engine.state.players.get_mut("A").unwrap().deck =
            vec![
                serde_json::from_value(json!({"atk_type": "Strike", "db_uuid": "d", "name": "d",
                "number": 1, "play_order": "Lead", "raw_text": "", "tags": [], "finish_bonuses": {},
                "effects": []}))
                .unwrap(),
            ];
        let before = engine.state.players["A"].hand.len();
        let v = engine
            .offer_roll_boost("A", Skill::Agility, 7, false)
            .unwrap();
        assert_eq!(v, 8, "the +1 branch boosts the roll");
        assert_eq!(
            engine.state.players["A"].hand.len(),
            before,
            "no draw on the boost branch"
        );
    }

    #[test]
    fn draw_branch_leaves_the_roll_and_draws() {
        let mut engine = engine(0); // pick "draw"
        engine.state.players.get_mut("A").unwrap().deck =
            vec![
                serde_json::from_value(json!({"atk_type": "Strike", "db_uuid": "d", "name": "d",
                "number": 1, "play_order": "Lead", "raw_text": "", "tags": [], "finish_bonuses": {},
                "effects": []}))
                .unwrap(),
            ];
        let before = engine.state.players["A"].hand.len();
        let v = engine
            .offer_roll_boost("A", Skill::Agility, 7, false)
            .unwrap();
        assert_eq!(v, 7, "the draw branch does not boost the roll");
        assert_eq!(engine.state.players["A"].hand.len(), before + 1, "drew 1");
    }

    #[test]
    fn does_not_fire_on_a_non_agility_roll() {
        let mut engine = engine(1);
        let v = engine
            .offer_roll_boost("A", Skill::Power, 7, false)
            .unwrap();
        assert_eq!(v, 7, "the Agility gate keeps it inert on a Power roll");
    }
}

/// Father Light (schema v55: ForceRevealPlay) — "When you roll Agility for your
/// turn roll, during your opponent's next turn, they randomly reveal a card in
/// their hand until they reveal a playable card; they must play that card."
#[cfg(test)]
mod father_light_tests {
    use super::*;
    use crate::policy::{HeuristicPolicy, Policies};
    use serde_json::{json, Value};

    fn card(uuid: &str, order: &str, number: i64) -> Value {
        json!({"atk_type": "Strike", "db_uuid": uuid, "name": uuid, "number": number,
               "play_order": order, "raw_text": "", "tags": [], "finish_bonuses": {}, "effects": []})
    }

    fn heuristic_pair() -> Policies {
        Policies::new(
            Box::new(HeuristicPolicy::heuristic()),
            Box::new(HeuristicPolicy::heuristic()),
        )
    }

    fn engine() -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let deck = |id: &str| -> Deck {
            serde_json::from_value(json!({
                "competitor": {"db_uuid": id, "name": id, "division": "World Championship",
                    "stats": stats, "effects": []},
                "entrance": {"db_uuid": format!("{id}-ent"), "name": "ent"}, "cards": [],
            }))
            .expect("deck")
        };
        Engine::new(
            deck("A"),
            deck("B"),
            Box::new(heuristic_pair()),
            1,
            String::new(),
            "sim".into(),
        )
    }

    fn hand(engine: &mut Engine, key: &str, cards: &[Value]) {
        engine.state.players.get_mut(key).unwrap().hand = cards
            .iter()
            .map(|c| serde_json::from_value(c.clone()).unwrap())
            .collect();
    }

    #[test]
    fn arming_sets_a_one_shot_flag_on_the_opponent_and_is_idempotent() {
        let mut engine = engine();
        engine.act_force_reveal_play(Who::Opp, "A"); // A's gimmick arms B
        assert!(engine.state.players["B"]
            .flags
            .contains_key("forced_reveal_play"));
        assert!(!engine.state.players["A"]
            .flags
            .contains_key("forced_reveal_play"));
        engine.act_force_reveal_play(Who::Opp, "A"); // re-arm: still armed exactly once
        assert!(engine.consume_forced_reveal_play("B"));
        assert!(
            !engine.consume_forced_reveal_play("B"),
            "consumed once, then clear"
        );
    }

    #[test]
    fn forces_the_only_playable_card_the_lead() {
        // B holds a Lead and a Follow Up with an empty board: only the Lead is
        // playable, so whatever the random reveal order, the Lead is force-played.
        let mut engine = engine();
        hand(
            &mut engine,
            "B",
            &[card("b-lead", "Lead", 1), card("b-fu", "Followup", 2)],
        );
        let played = engine.forced_reveal_and_play("B", "A").unwrap();
        assert!(played, "a playable card was forced");
        let b = &engine.state.players["B"];
        assert!(
            b.in_play.iter().any(|c| c.db_uuid == "b-lead"),
            "the Lead landed"
        );
        assert_eq!(b.hand.len(), 1, "only the Follow Up remains in hand");
        assert!(b.hand.iter().any(|c| c.db_uuid == "b-fu"));
    }

    #[test]
    fn nothing_playable_reveals_the_whole_hand_and_plays_nothing() {
        // All Follow Ups and Finishes, no Lead in play: nothing is playable, so the
        // whole hand is revealed and no card is played (returns false → the turn
        // falls through to the ordinary pass).
        let mut engine = engine();
        hand(
            &mut engine,
            "B",
            &[card("b-fu", "Followup", 1), card("b-fin", "Finish", 2)],
        );
        let played = engine.forced_reveal_and_play("B", "A").unwrap();
        assert!(!played, "no playable card");
        let b = &engine.state.players["B"];
        assert_eq!(b.hand.len(), 2, "the hand is untouched");
        assert!(b.in_play.is_empty());
    }

    #[test]
    fn take_turn_action_consumes_the_armed_forced_play() {
        // The full wiring: an armed B, on taking its turn, is forced to play its
        // only playable card (the Lead) and the flag is consumed.
        let mut engine = engine();
        engine.act_force_reveal_play(Who::Opp, "A");
        hand(&mut engine, "B", &[card("b-lead", "Lead", 1)]);
        engine.state.active = "B".into();
        engine.take_turn_action("B").unwrap();
        let b = &engine.state.players["B"];
        assert!(
            b.in_play.iter().any(|c| c.db_uuid == "b-lead"),
            "forced to play the Lead"
        );
        assert!(!b.flags.contains_key("forced_reveal_play"), "flag consumed");
    }
}

/// The Magnificient Mr. Rey (schema v56: GrantSwapNextTurn) — "When you roll
/// Technique for your turn roll: Once on the next turn, you may switch 1 card in
/// your hand with 1 card in your discard pile."
#[cfg(test)]
mod mr_rey_tests {
    use super::*;
    use serde_json::{json, Value};

    /// Always says "yes" to the optional swap and picks the first card at each
    /// zone pick (so the swap is deterministic).
    struct AlwaysSwap;
    impl Decider for AlwaysSwap {
        fn decide(
            &mut self,
            _: &str,
            _: &str,
            legal: &[Value],
            _: &mut GameState,
        ) -> Option<Value> {
            legal.first().cloned()
        }
        fn policy_name(&self, _: &str) -> String {
            "always-swap".to_owned()
        }
    }

    fn card(uuid: &str, number: i64) -> Value {
        json!({"atk_type": "Strike", "db_uuid": uuid, "name": uuid, "number": number,
               "play_order": "Lead", "raw_text": "", "tags": [], "finish_bonuses": {}, "effects": []})
    }

    fn engine() -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let deck = |id: &str| -> Deck {
            serde_json::from_value(json!({
                "competitor": {"db_uuid": id, "name": id, "division": "World Championship",
                    "stats": stats, "effects": []},
                "entrance": {"db_uuid": format!("{id}-ent"), "name": "ent"}, "cards": [],
            }))
            .expect("deck")
        };
        Engine::new(
            deck("A"),
            deck("B"),
            Box::new(AlwaysSwap),
            1,
            String::new(),
            "sim".into(),
        )
    }

    #[test]
    fn grant_arms_next_then_promotes_to_this_then_expires_if_unused() {
        let mut engine = engine();
        engine.act_grant_swap_next_turn(Who::SelfSide, "A"); // A grants itself
        assert!(engine.state.players["A"]
            .flags
            .contains_key("swap_grant_next"));
        engine.promote_swap_grant("A"); // next -> this (usable this turn)
        assert!(engine.state.players["A"]
            .flags
            .contains_key("swap_grant_this"));
        assert!(!engine.state.players["A"]
            .flags
            .contains_key("swap_grant_next"));
        engine.promote_swap_grant("A"); // unused -> expires (SET, not accumulate)
        assert!(!engine.state.players["A"]
            .flags
            .contains_key("swap_grant_this"));
    }

    #[test]
    fn offer_performs_the_swap_when_the_grant_is_usable() {
        let mut engine = engine();
        engine.state.players.get_mut("A").unwrap().hand =
            vec![serde_json::from_value(card("h1", 1)).unwrap()];
        engine.state.players.get_mut("A").unwrap().discard =
            vec![serde_json::from_value(card("d1", 2)).unwrap()];
        engine
            .state
            .players
            .get_mut("A")
            .unwrap()
            .flags
            .insert("swap_grant_this".into(), json!(true));
        engine.offer_swap_grant("A").unwrap();
        let a = &engine.state.players["A"];
        assert!(
            a.hand.iter().any(|c| c.db_uuid == "d1"),
            "the discard card came into hand"
        );
        assert!(
            a.discard.iter().any(|c| c.db_uuid == "h1"),
            "the hand card went to discard"
        );
        assert!(!a.flags.contains_key("swap_grant_this"), "grant consumed");
    }

    #[test]
    fn offer_is_a_noop_without_a_usable_grant() {
        let mut engine = engine();
        engine.state.players.get_mut("A").unwrap().hand =
            vec![serde_json::from_value(card("h1", 1)).unwrap()];
        engine.state.players.get_mut("A").unwrap().discard =
            vec![serde_json::from_value(card("d1", 2)).unwrap()];
        engine.offer_swap_grant("A").unwrap(); // no swap_grant_this set
        assert!(
            engine.state.players["A"]
                .hand
                .iter()
                .any(|c| c.db_uuid == "h1"),
            "unchanged"
        );
    }

    #[test]
    fn empty_discard_consumes_the_grant_without_a_swap() {
        let mut engine = engine();
        engine.state.players.get_mut("A").unwrap().hand =
            vec![serde_json::from_value(card("h1", 1)).unwrap()];
        engine
            .state
            .players
            .get_mut("A")
            .unwrap()
            .flags
            .insert("swap_grant_this".into(), json!(true));
        engine.offer_swap_grant("A").unwrap();
        let a = &engine.state.players["A"];
        assert_eq!(a.hand.len(), 1, "nothing to swap into");
        assert!(
            !a.flags.contains_key("swap_grant_this"),
            "window still passes (consumed)"
        );
    }
}

/// The SRG Boss (The Greatest American Who Ever Lived) (schema v57: AbsorbGimmick)
/// — "At the start of the match reveal any number of Singles Competitors with the
/// SRG Boss (V1) logo: Choose 1 and add their Gimmick to yours."
#[cfg(test)]
mod srg_boss_tests {
    use super::*;
    use crate::conditions::RollContext;
    use crate::policy::{HeuristicPolicy, Policies};
    use serde_json::{json, Value};

    fn heuristic_pair() -> Policies {
        Policies::new(
            Box::new(HeuristicPolicy::heuristic()),
            Box::new(HeuristicPolicy::heuristic()),
        )
    }

    fn engine() -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let deck = |id: &str| -> Deck {
            serde_json::from_value(json!({
                "competitor": {"db_uuid": id, "name": id, "division": "World Championship",
                    "stats": stats, "effects": []},
                "entrance": {"db_uuid": format!("{id}-ent"), "name": "ent"}, "cards": [],
            }))
            .expect("deck")
        };
        Engine::new(
            deck("A"),
            deck("B"),
            Box::new(heuristic_pair()),
            1,
            String::new(),
            "sim".into(),
        )
    }

    /// An absorbable gimmick: OnRoll(Agility) -> Draw 1.
    fn onroll_draw() -> Effect {
        serde_json::from_value(json!({
            "@type": "Effect",
            "trigger": {"@type": "OnRoll", "skill": "Agility", "who": "SELF"},
            "condition": {"@type": "Always"},
            "actions": [{"@type": "Draw", "n": 1, "source": "TOP", "who": "SELF",
                         "cap": null, "per": null, "per_excludes_trigger": false, "per_who": "SELF"}],
            "duration": "INSTANT",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "absorbed", "source": "gimmick", "optional": false
        }))
        .unwrap()
    }

    fn a_card(uuid: &str) -> Value {
        json!({"atk_type": "Strike", "db_uuid": uuid, "name": uuid, "number": 1,
               "play_order": "Lead", "raw_text": "", "tags": [], "finish_bonuses": {}, "effects": []})
    }

    #[test]
    fn absorb_appends_the_gimmick_and_it_becomes_standing() {
        let mut engine = engine();
        assert_eq!(engine.standing_effects("A").len(), 0);
        engine.act_absorb_gimmick(&[onroll_draw()], "A");
        assert_eq!(
            engine.state.players["A"].competitor.effects.len(),
            1,
            "added to the gimmick"
        );
        assert_eq!(
            engine.standing_effects("A").len(),
            1,
            "and it is now standing"
        );
    }

    #[test]
    fn absorbed_effect_fires_as_a_standing_gimmick() {
        let mut engine = engine();
        engine.state.players.get_mut("A").unwrap().deck =
            vec![serde_json::from_value(a_card("c1")).unwrap()];
        engine.act_absorb_gimmick(&[onroll_draw()], "A");
        engine.roll_ctx.insert(
            "A".into(),
            RollContext {
                skill: Some(Skill::Agility),
                value: Some(7),
                ..Default::default()
            },
        );
        engine.run_on_roll("A").unwrap();
        assert_eq!(
            engine.state.players["A"].hand.len(),
            1,
            "absorbed OnRoll->Draw fired"
        );
    }

    #[test]
    fn absorbing_multiple_effects_adds_them_all() {
        let mut engine = engine();
        engine.act_absorb_gimmick(&[onroll_draw(), onroll_draw()], "A"); // V2 = 2 effects
        assert_eq!(engine.standing_effects("A").len(), 2);
    }
}

/// El Ganso Ruso (schema v58: CopyEntrance) — "Copy your target's Entrance or
/// your 1st turn roll is +6."
#[cfg(test)]
mod el_ganso_ruso_tests {
    use super::*;
    use crate::conditions::RollContext;
    use crate::policy::{HeuristicPolicy, Policies};
    use serde_json::{json, Value};

    fn heuristic_pair() -> Policies {
        Policies::new(
            Box::new(HeuristicPolicy::heuristic()),
            Box::new(HeuristicPolicy::heuristic()),
        )
    }

    fn engine() -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let deck = |id: &str| -> Deck {
            serde_json::from_value(json!({
                "competitor": {"db_uuid": id, "name": id, "division": "World Championship",
                    "stats": stats, "effects": []},
                "entrance": {"db_uuid": format!("{id}-ent"), "name": "ent"}, "cards": [],
            }))
            .expect("deck")
        };
        Engine::new(
            deck("A"),
            deck("B"),
            Box::new(heuristic_pair()),
            1,
            String::new(),
            "sim".into(),
        )
    }

    /// An ongoing entrance ability: OnRoll(Agility) -> Draw 1.
    fn onroll_draw() -> Effect {
        serde_json::from_value(json!({
            "@type": "Effect",
            "trigger": {"@type": "OnRoll", "skill": "Agility", "who": "SELF"},
            "condition": {"@type": "Always"},
            "actions": [{"@type": "Draw", "n": 1, "source": "TOP", "who": "SELF",
                         "cap": null, "per": null, "per_excludes_trigger": false, "per_who": "SELF"}],
            "duration": "INSTANT",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "opp-entrance", "source": "entrance", "optional": false
        }))
        .unwrap()
    }

    fn a_card(uuid: &str) -> Value {
        json!({"atk_type": "Strike", "db_uuid": uuid, "name": uuid, "number": 1,
               "play_order": "Lead", "raw_text": "", "tags": [], "finish_bonuses": {}, "effects": []})
    }

    #[test]
    fn copy_appends_the_targets_entrance_effects() {
        let mut engine = engine();
        engine.state.players.get_mut("B").unwrap().entrance.effects = vec![onroll_draw()];
        engine.act_copy_entrance(Who::Opp, "A"); // A copies B's entrance
        assert_eq!(
            engine.state.players["A"].entrance.effects.len(),
            1,
            "A gained B's entrance ability"
        );
        assert_eq!(
            engine.state.players["B"].entrance.effects.len(),
            1,
            "B's own entrance untouched"
        );
    }

    #[test]
    fn a_copied_ongoing_entrance_ability_fires_for_the_copier() {
        let mut engine = engine();
        engine.state.players.get_mut("A").unwrap().deck =
            vec![serde_json::from_value(a_card("c1")).unwrap()];
        engine.state.players.get_mut("B").unwrap().entrance.effects = vec![onroll_draw()];
        engine.act_copy_entrance(Who::Opp, "A");
        engine.roll_ctx.insert(
            "A".into(),
            RollContext {
                skill: Some(Skill::Agility),
                value: Some(7),
                ..Default::default()
            },
        );
        engine.run_on_roll("A").unwrap();
        assert_eq!(
            engine.state.players["A"].hand.len(),
            1,
            "copied OnRoll->Draw fired for A"
        );
    }

    #[test]
    fn copying_your_own_entrance_is_a_noop() {
        let mut engine = engine();
        engine.state.players.get_mut("A").unwrap().entrance.effects = vec![onroll_draw()];
        engine.act_copy_entrance(Who::SelfSide, "A"); // SELF -> no-op (no doubling)
        assert_eq!(engine.state.players["A"].entrance.effects.len(), 1);
    }
}

#[cfg(test)]
mod gm_calace_tests {
    use super::*;
    use crate::policy::{HeuristicPolicy, Policies};
    use serde_json::{json, Value};

    fn heuristic_pair() -> Policies {
        Policies::new(
            Box::new(HeuristicPolicy::heuristic()),
            Box::new(HeuristicPolicy::heuristic()),
        )
    }

    fn engine() -> Engine {
        let stats =
            json!({"Power":5,"Agility":5,"Technique":5,"Submission":5,"Grapple":5,"Strike":5});
        let deck = |id: &str| -> Deck {
            serde_json::from_value(json!({
                "competitor": {"db_uuid": id, "name": id, "division": "World Championship",
                    "stats": stats, "effects": []},
                "entrance": {"db_uuid": format!("{id}-ent"), "name": "ent"}, "cards": [],
            }))
            .expect("deck")
        };
        Engine::new(
            deck("A"),
            deck("B"),
            Box::new(heuristic_pair()),
            1,
            String::new(),
            "sim".into(),
        )
    }

    /// A Static match-rule declaration carrying `actions`, shaped like the effects a
    /// `SwapCrowdMeter` installs into the Entrance.
    fn static_rule(actions: Value) -> Effect {
        serde_json::from_value(json!({
            "@type": "Effect",
            "trigger": {"@type": "Static"},
            "condition": {"@type": "Always"},
            "actions": actions,
            "duration": "WHILE_IN_PLAY",
            "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
            "raw_clause": "match rule", "source": "gimmick", "optional": false
        }))
        .unwrap()
    }

    /// The No DQ Match bundle: Match-scoped DQ and count-out rules both disabled.
    fn no_dq_rules() -> Vec<Effect> {
        vec![static_rule(json!([
            {"@type": "DisqualificationRule", "enabled": false, "scope": "MATCH"},
            {"@type": "CountOutRule", "enabled": false, "scope": "MATCH"}
        ]))]
    }

    #[test]
    fn swap_installs_rules_into_the_owners_entrance() {
        let mut engine = engine();
        let effects = no_dq_rules();
        engine.act_swap_crowd_meter("No DQ Match", &effects, "A");
        assert_eq!(
            engine.state.players["A"].entrance.effects.len(),
            1,
            "installed into A's entrance"
        );
        assert!(
            engine.state.players["B"].entrance.effects.is_empty(),
            "opponent's entrance untouched"
        );
    }

    #[test]
    fn a_match_scoped_swap_reaches_both_players() {
        let mut engine = engine();
        let effects = no_dq_rules();
        engine.act_swap_crowd_meter("No DQ Match", &effects, "A"); // installed on A only
        for who in ["A", "B"] {
            assert!(engine.is_dq_immune(who), "{who} DQ-immune");
            assert!(engine.is_count_out_immune(who), "{who} count-out-immune");
        }
    }

    #[test]
    fn no_count_outs_survives_an_empty_deck_and_hand() {
        // Baseline: emptying deck+hand on a won turn ends the match by count-out.
        let mut base = engine();
        base.state.players.get_mut("A").unwrap().deck.clear();
        base.state.players.get_mut("A").unwrap().hand.clear();
        assert!(
            !base.draw_for_turn("A").unwrap(),
            "no rule: count-out ends the game"
        );

        // With a No Count Outs match type installed, play continues instead.
        let mut engine = engine();
        engine.state.players.get_mut("A").unwrap().deck.clear();
        engine.state.players.get_mut("A").unwrap().hand.clear();
        let effects = no_dq_rules();
        engine.act_swap_crowd_meter("No DQ Match", &effects, "A");
        assert!(
            engine.draw_for_turn("A").unwrap(),
            "No Count Outs: play continues with nothing to draw"
        );
    }

    #[test]
    fn steel_cage_caps_both_players_hands() {
        let mut engine = engine();
        // Steel Cage "Max Handsize: 6" = a -4 delta from the base 10 on BOTH players.
        let effects = vec![static_rule(json!([
            {"@type": "MaxHandSize", "delta": -4, "who": "SELF", "duration": "WHILE_IN_PLAY"},
            {"@type": "MaxHandSize", "delta": -4, "who": "OPP", "duration": "WHILE_IN_PLAY"}
        ]))];
        engine.act_swap_crowd_meter("Steel Cage Match", &effects, "A");
        assert_eq!(engine.state.effective_hand_cap("A", HAND_CAP, None), 6);
        assert_eq!(engine.state.effective_hand_cap("B", HAND_CAP, None), 6);
    }

    #[test]
    fn is_match_type_reads_the_stipulation() {
        use crate::ir::MatchType;
        let mut engine = engine();
        let gate = Condition::IsMatchType {
            types: vec![MatchType::SteelCage, MatchType::LigersDen],
        };
        // Default Standard match: the gate is inert.
        assert!(!conditions::holds(&gate, &engine.state, "A", None));
        // In a Steel Cage match, the OR-set holds; a Liger's Den match too.
        engine.state.match_type = MatchType::SteelCage;
        assert!(conditions::holds(&gate, &engine.state, "A", None));
        engine.state.match_type = MatchType::LigersDen;
        assert!(conditions::holds(&gate, &engine.state, "A", None));
        // A Triad match is not in the set.
        engine.state.match_type = MatchType::Triad;
        assert!(!conditions::holds(&gate, &engine.state, "A", None));
    }

    /// `RevealThen` peeks the deck top and, on a name match, moves that card to the
    /// owner's hand ("add that card to your hand"); a non-matching top is left in place.
    #[test]
    fn reveal_then_takes_matched_deck_card_to_hand() {
        let card = |name: &str| -> Card {
            serde_json::from_value(json!({
                "atk_type": "Strike", "db_uuid": name, "effects": [],
                "finish_bonuses": {}, "name": name, "number": 1,
                "play_order": "Lead", "raw_text": "", "tags": []
            }))
            .expect("card")
        };
        let take = |name_frag: &str| Action::RevealThen {
            reveal_from: RevealSource::DeckTop,
            count: 1,
            filter: CardFilter {
                name_contains: vec![name_frag.to_owned()],
                ..Default::default()
            },
            take_matched: true,
            then: Vec::new(),
            then_optional: false,
        };
        let mut e = engine();
        {
            let d = &mut e.state.players.get_mut("A").unwrap().deck;
            d.clear();
            d.push(card("Barbed Wire Bat")); // top
            d.push(card("Plain Jane"));
        }
        // Match on top -> the card is pulled to hand; the card beneath stays on the deck.
        e.apply_action(&take("Barbed Wire"), "A", "").unwrap();
        let p = &e.state.players["A"];
        assert!(p.hand.iter().any(|c| c.db_uuid == "Barbed Wire Bat"));
        assert_eq!(p.deck.len(), 1);
        assert_eq!(p.deck[0].db_uuid, "Plain Jane");

        // No match on the new top -> nothing moves.
        let hand_before = e.state.players["A"].hand.len();
        e.apply_action(&take("Nonexistent"), "A", "").unwrap();
        assert_eq!(e.state.players["A"].hand.len(), hand_before);
        assert_eq!(e.state.players["A"].deck.len(), 1);
    }
}
