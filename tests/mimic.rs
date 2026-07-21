//! Mimic (task #107): `MirrorOpponentIncrease` — "when your opponent increases their
//! skills, your skills are also increased the same amount." A derived-stats fold: the
//! declarer gains the positive part of the opponent's `effective - base` per skill.
//! Driven against the real `bull_fae_fresh` position.

use serde_json::{json, Value};
use srg_core::ir::Skill;
use srg_core::state::GameState;
use std::path::PathBuf;

/// The `bull_fae_fresh` state with `a_extra`/`b_extra` spliced onto each competitor.
fn state_with(a_extra: &[Value], b_extra: &[Value]) -> GameState {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/state/positions.json");
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mut state = doc["positions"][0]["state"].clone();
    for (seat, extra) in [("A", a_extra), ("B", b_extra)] {
        state["players"][seat]["competitor"]["effects"]
            .as_array_mut()
            .unwrap()
            .extend(extra.iter().cloned());
    }
    GameState::from_dict(state).expect("from_dict")
}

fn mirror() -> Value {
    json!({
        "@type": "Effect",
        "trigger": {"@type": "Static"},
        "condition": {"@type": "Always"},
        "actions": [{"@type": "MirrorOpponentIncrease"}],
        "duration": "WHILE_IN_PLAY",
        "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
        "raw_clause": "test", "source": "gimmick", "optional": false
    })
}

/// A Static self-buff `+delta` to `skill`.
fn self_buff(skill: &str, delta: i64) -> Value {
    json!({
        "@type": "Effect",
        "trigger": {"@type": "Static"},
        "condition": {"@type": "Always"},
        "actions": [{"@type": "BuffSkill", "skill": skill, "delta": delta, "who": "SELF",
                     "duration": "WHILE_IN_PLAY", "target_highest": false, "per_crowd": false,
                     "cap": null, "per": null, "per_zone": "IN_PLAY"}],
        "duration": "WHILE_IN_PLAY",
        "frequency": {"@type": "FrequencyGuard", "kind": "UNLIMITED", "n": null},
        "raw_clause": "test", "source": "gimmick", "optional": false
    })
}

#[test]
fn mirror_adds_the_opponents_skill_increase() {
    let base = state_with(&[], &[self_buff("Power", 3)]);
    let a_power = base.effective_stat("A", Skill::Power, None);
    // A now declares the mirror; B's +3 Power is echoed onto A.
    let mimic = state_with(&[mirror()], &[self_buff("Power", 3)]);
    assert_eq!(mimic.effective_stat("A", Skill::Power, None), a_power + 3);
    // B itself is unchanged by A's declaration.
    assert_eq!(
        mimic.effective_stat("B", Skill::Power, None),
        base.effective_stat("B", Skill::Power, None)
    );
}

#[test]
fn mirror_is_inert_without_the_declaration() {
    let with = state_with(&[], &[self_buff("Agility", 4)]);
    let without_mirror = with.effective_stat("A", Skill::Agility, None);
    let plain = state_with(&[], &[]);
    assert_eq!(
        without_mirror,
        plain.effective_stat("A", Skill::Agility, None)
    );
}

#[test]
fn a_decrease_is_not_mirrored() {
    // "increases their skills" — only positive deltas echo. B's -3 Strike is ignored.
    let plain = state_with(&[mirror()], &[]);
    let a_strike = plain.effective_stat("A", Skill::Strike, None);
    let debuffed = state_with(&[mirror()], &[self_buff("Strike", -3)]);
    assert_eq!(debuffed.effective_stat("A", Skill::Strike, None), a_strike);
}

#[test]
fn mirror_is_per_skill() {
    // B raises Power +2 and Grapple +5; A mirrors each independently.
    let base = state_with(&[], &[self_buff("Power", 2), self_buff("Grapple", 5)]);
    let mimic = state_with(
        &[mirror()],
        &[self_buff("Power", 2), self_buff("Grapple", 5)],
    );
    assert_eq!(
        mimic.effective_stat("A", Skill::Power, None),
        base.effective_stat("A", Skill::Power, None) + 2
    );
    assert_eq!(
        mimic.effective_stat("A", Skill::Grapple, None),
        base.effective_stat("A", Skill::Grapple, None) + 5
    );
}
