"""Tests for GameState / PlayerState: zones, derived stats, snapshots (§5)."""

from __future__ import annotations

from srg_sim.cards import AtkType, Card, Competitor, EntranceCard, PlayOrder, Skill, Stats
from srg_sim.effects import (
    Always,
    BuffSkill,
    Comparator,
    CrowdMeterCompare,
    Duration,
    Effect,
    EffectSource,
    MaxHandSize,
    MinHandSize,
    MirrorOpponentIncrease,
    OnPlay,
    Static,
    Who,
)
from srg_sim.rng import SeededRNG
from srg_sim.state import GameState, PlayerState

BULL_STATS = Stats(power=10, technique=6, agility=5, submission=8, grapple=9, strike=7)
FAE_STATS = Stats(power=10, technique=7, agility=6, submission=9, grapple=5, strike=8)


def _static_buff(skill: Skill, delta: int, who: Who, src: EffectSource) -> Effect:
    dur = Duration.WHILE_GIMMICK_ACTIVE if src is EffectSource.GIMMICK else Duration.WHILE_IN_PLAY
    return Effect(
        trigger=Static(),
        actions=(BuffSkill(skill=skill, delta=delta, who=who, duration=dur),),
        duration=dur,
        source=src,
    )


def _card(number: int, effects: tuple[Effect, ...] = ()) -> Card:
    return Card(
        db_uuid=f"u-{number}",
        name=f"Card {number}",
        number=number,
        atk_type=AtkType.STRIKE,
        play_order=PlayOrder.LEAD,
        effects=effects,
    )


def _state(
    a_comp_effects: tuple[Effect, ...] = (),
    a_in_play: tuple[Card, ...] = (),
    b_comp_effects: tuple[Effect, ...] = (),
) -> GameState:
    ent = EntranceCard("u-ent", "Entrance")
    bull = Competitor("u-bull", "The Bull", "Worlds", BULL_STATS, effects=a_comp_effects)
    fae = Competitor("u-fae", "Fae Dragon", "Worlds", FAE_STATS, effects=b_comp_effects)
    a = PlayerState(competitor=bull, entrance=ent, in_play=list(a_in_play))
    b = PlayerState(competitor=fae, entrance=ent)
    return GameState(players={"A": a, "B": b}, rng=SeededRNG(1))


def test_base_stats_when_no_buffs() -> None:
    gs = _state()
    assert gs.effective_stats("A") == BULL_STATS.to_dict()
    assert gs.effective_stats("B") == FAE_STATS.to_dict()


def test_gimmick_self_buff_and_opp_debuff() -> None:
    gs = _state(
        a_comp_effects=(
            _static_buff(Skill.POWER, 2, Who.SELF, EffectSource.GIMMICK),
            _static_buff(Skill.STRIKE, -1, Who.OPP, EffectSource.GIMMICK),
        )
    )
    assert gs.effective_stat("A", Skill.POWER) == 12
    assert gs.effective_stat("B", Skill.STRIKE) == 7  # 8 - 1 from Bull's OPP debuff


def test_blanked_gimmick_drops_all_its_buffs() -> None:
    gs = _state(a_comp_effects=(_static_buff(Skill.POWER, 2, Who.SELF, EffectSource.GIMMICK),))
    gs.players["A"].gimmick_blanked = True
    assert gs.effective_stat("A", Skill.POWER) == 10


def test_card_in_play_buff_folds_in_and_out() -> None:
    card = _card(1, (_static_buff(Skill.GRAPPLE, 3, Who.SELF, EffectSource.CARD),))
    gs = _state(a_in_play=(card,))
    assert gs.effective_stat("A", Skill.GRAPPLE) == 12
    gs.players["A"].in_play.clear()  # card leaves play -> buff gone
    assert gs.effective_stat("A", Skill.GRAPPLE) == 9


def _mirror() -> Effect:
    return Effect(
        trigger=Static(),
        actions=(MirrorOpponentIncrease(),),
        duration=Duration.WHILE_IN_PLAY,
    )


def test_mimic_mirrors_opponent_skill_increase() -> None:
    # B raises its own Power +3; A (Mimic) gains the same +3. B is unchanged.
    b_buff = _static_buff(Skill.POWER, 3, Who.SELF, EffectSource.GIMMICK)
    base = _state(b_comp_effects=(b_buff,))
    a_power = base.effective_stat("A", Skill.POWER)
    mimic = _state(a_comp_effects=(_mirror(),), b_comp_effects=(b_buff,))
    assert mimic.effective_stat("A", Skill.POWER) == a_power + 3
    assert mimic.effective_stat("B", Skill.POWER) == base.effective_stat("B", Skill.POWER)


def test_mimic_ignores_a_decrease() -> None:
    # "increases their skills" — a debuff on B is not mirrored onto A.
    plain = _state(a_comp_effects=(_mirror(),))
    a_strike = plain.effective_stat("A", Skill.STRIKE)
    debuffed = _state(
        a_comp_effects=(_mirror(),),
        b_comp_effects=(_static_buff(Skill.STRIKE, -3, Who.SELF, EffectSource.GIMMICK),),
    )
    assert debuffed.effective_stat("A", Skill.STRIKE) == a_strike


def test_mimic_is_inert_without_the_declaration() -> None:
    b_buff = _static_buff(Skill.AGILITY, 4, Who.SELF, EffectSource.GIMMICK)
    with_buff = _state(b_comp_effects=(b_buff,))
    plain = _state()
    assert with_buff.effective_stat("A", Skill.AGILITY) == plain.effective_stat("A", Skill.AGILITY)


def test_only_static_trigger_folds_into_derived_stats() -> None:
    # A BuffSkill under OnPlay (a one-shot) must NOT show up in derived stats.
    one_shot = Effect(
        trigger=OnPlay(),
        actions=(BuffSkill(skill=Skill.POWER, delta=5, who=Who.SELF),),
    )
    gs = _state(a_in_play=(_card(1, (one_shot,)),))
    assert gs.effective_stat("A", Skill.POWER) == 10


def test_conditional_buff_needs_holds_evaluator() -> None:
    cond = Effect(
        trigger=Static(),
        condition=CrowdMeterCompare(cmp=Comparator.GE, value=1),
        actions=(BuffSkill(skill=Skill.POWER, delta=4, who=Who.SELF),),
        duration=Duration.WHILE_IN_PLAY,
    )
    gs = _state(a_in_play=(_card(1, (cond,)),))
    # No evaluator -> conditional buff withheld.
    assert gs.effective_stat("A", Skill.POWER) == 10
    # Evaluator that says the condition holds -> buff applies.
    assert gs.effective_stat("A", Skill.POWER, holds=lambda c: True) == 14


def test_unconditional_buff_ignores_holds() -> None:
    gs = _state(a_in_play=(_card(1, (_static_buff(Skill.POWER, 1, Who.SELF, EffectSource.CARD),)),))
    assert gs.effective_stat("A", Skill.POWER, holds=lambda c: False) == 11


def _static_hand_mod(delta: int, who: Who) -> Effect:
    return Effect(
        trigger=Static(),
        actions=(MaxHandSize(delta=delta, who=who),),
        duration=Duration.WHILE_IN_PLAY,
    )


def test_hand_cap_is_base_with_no_modifiers() -> None:
    gs = _state()
    assert gs.effective_hand_cap("A", 10) == 10
    assert gs.effective_hand_cap("B", 10) == 10


def test_gimmick_self_raise_and_opp_lower_hand_cap() -> None:
    # Bull's gimmick raises its own cap +1 and drops the opponent's -2.
    gs = _state(a_comp_effects=(_static_hand_mod(1, Who.SELF), _static_hand_mod(-2, Who.OPP)))
    assert gs.effective_hand_cap("A", 10) == 11
    assert gs.effective_hand_cap("B", 10) == 8


def test_hand_cap_mod_folds_in_and_out_with_card() -> None:
    card = _card(1, (_static_hand_mod(2, Who.SELF),))
    gs = _state(a_in_play=(card,))
    assert gs.effective_hand_cap("A", 10) == 12
    gs.players["A"].in_play.clear()  # card leaves play -> cap back to base
    assert gs.effective_hand_cap("A", 10) == 10


def test_blanked_gimmick_drops_its_hand_cap_mod() -> None:
    gs = _state(a_comp_effects=(_static_hand_mod(-3, Who.OPP),))
    assert gs.effective_hand_cap("B", 10) == 7
    gs.players["A"].gimmick_blanked = True
    assert gs.effective_hand_cap("B", 10) == 10


def test_hand_cap_floored_at_default_minimum() -> None:
    # A max-handsize reduction can never drop the cap below the default minimum (3),
    # per the SRG ruling — not to zero. -20 would give -10; the floor holds it at 3.
    gs = _state(a_comp_effects=(_static_hand_mod(-20, Who.OPP),))
    assert gs.effective_hand_cap("B", 10) == 3


def _static_min_hand_mod(delta: int, who: Who) -> Effect:
    return Effect(
        trigger=Static(),
        actions=(MinHandSize(delta=delta, who=who),),
        duration=Duration.WHILE_IN_PLAY,
    )


def test_min_handsize_raises_floor_on_reduced_cap() -> None:
    # Quadruple H raises the minimum +2 (floor 5). An opponent reducing the max to 4
    # is clamped up to that floor.
    gs = _state(
        a_comp_effects=(_static_min_hand_mod(2, Who.SELF),),
        b_comp_effects=(_static_hand_mod(-6, Who.OPP),),  # A's max: 10-6 = 4
    )
    assert gs.effective_hand_cap("A", 10) == 5


def test_min_handsize_alone_does_not_lower_a_healthy_cap() -> None:
    # Minimum above default but below the (unreduced) maximum: no effect on the cap.
    gs = _state(a_comp_effects=(_static_min_hand_mod(2, Who.SELF),))
    assert gs.effective_hand_cap("A", 10) == 10


def test_quadruple_h_min_and_max_plus_two() -> None:
    # Both +2: max 12, min floor 5 -> cap = max(12, 5) = 12.
    gs = _state(
        a_comp_effects=(_static_hand_mod(2, Who.SELF), _static_min_hand_mod(2, Who.SELF)),
    )
    assert gs.effective_hand_cap("A", 10) == 12


def test_min_above_max_becomes_new_max() -> None:
    # Minimum raised above a reduced maximum becomes the new maximum: max -6 (=4),
    # min +4 (floor 7) -> cap 7.
    gs = _state(
        a_comp_effects=(_static_min_hand_mod(4, Who.SELF),),
        b_comp_effects=(_static_hand_mod(-6, Who.OPP),),
    )
    assert gs.effective_hand_cap("A", 10) == 7


def test_draw_moves_top_of_deck_to_hand() -> None:
    gs = _state()
    gs.players["A"].deck = [_card(i) for i in range(1, 6)]
    drawn = gs.players["A"].draw(2)
    assert [c.number for c in drawn] == [1, 2]
    assert [c.number for c in gs.players["A"].hand] == [1, 2]
    assert [c.number for c in gs.players["A"].deck] == [3, 4, 5]


def test_draw_past_end_takes_what_is_left() -> None:
    gs = _state()
    gs.players["A"].deck = [_card(1)]
    assert len(gs.players["A"].draw(5)) == 1
    assert gs.players["A"].deck == []


def test_opponent_of() -> None:
    gs = _state()
    assert gs.opponent_of("A") == "B"
    assert gs.opponent_of("B") == "A"


def test_snapshot_round_trip_preserves_everything() -> None:
    gs = _state(a_comp_effects=(_static_buff(Skill.POWER, 2, Who.SELF, EffectSource.GIMMICK),))
    gs.players["A"].deck = [_card(i) for i in range(1, 4)]
    gs.players["B"].hand = [_card(9)]
    gs.crowd_meter = 3
    gs.active = "B"
    gs.turn_no = 7
    [gs.rng.roll() for _ in range(4)]  # advance RNG so its state is non-initial

    data = gs.to_dict()
    restored = GameState.from_dict(data)
    assert restored.to_dict() == data
    assert restored.effective_stats("A") == gs.effective_stats("A")
    # RNG resumes bit-exact through the snapshot.
    assert [restored.rng.roll() for _ in range(6)] == [gs.rng.roll() for _ in range(6)]


def test_snapshot_excludes_log() -> None:
    gs = _state()
    assert "log" not in gs.to_dict()


def test_condition_default_always_applies() -> None:
    # Effect() defaults condition to Always(); such a buff needs no evaluator.
    eff = Effect(
        trigger=Static(),
        condition=Always(),
        actions=(BuffSkill(skill=Skill.AGILITY, delta=1, who=Who.SELF),),
        duration=Duration.WHILE_IN_PLAY,
    )
    gs = _state(a_in_play=(_card(1, (eff,)),))
    assert gs.effective_stat("A", Skill.AGILITY) == 6


# --- observation model (§7 / todo #34) -------------------------------------


def _populated_state() -> GameState:
    ent = EntranceCard("u-ent", "Entrance")
    bull = Competitor("u-bull", "The Bull", "Worlds", BULL_STATS)
    fae = Competitor("u-fae", "Fae Dragon", "Worlds", FAE_STATS)
    a = PlayerState(
        competitor=bull,
        entrance=ent,
        hand=[_card(1), _card(2)],
        deck=[_card(3), _card(4), _card(5)],
        discard=[_card(6)],
        in_play=[_card(7)],
    )
    b = PlayerState(
        competitor=fae,
        entrance=ent,
        hand=[_card(11), _card(12), _card(13)],
        deck=[_card(14)],
        discard=[_card(15), _card(16)],
        in_play=[_card(17)],
    )
    gs = GameState(players={"A": a, "B": b}, rng=SeededRNG(1))
    gs.crowd_meter, gs.active, gs.turn_no = 2, "B", 4
    return gs


def test_observable_reveals_own_hand_but_only_opponent_hand_size() -> None:
    view = _populated_state().observable("A")
    a_hand = [c["db_uuid"] for c in view["players"]["A"]["hand"]]
    assert a_hand == ["u-1", "u-2"]  # own hand: full contents
    assert "hand" not in view["players"]["B"]  # opponent hand: hidden
    assert view["players"]["B"]["hand_size"] == 3  # only the count leaks


def test_observable_hides_every_deck_to_a_size() -> None:
    view = _populated_state().observable("A")
    # Deck order is hidden from everyone, owner included: only sizes, no contents.
    assert view["players"]["A"]["deck_size"] == 3
    assert view["players"]["B"]["deck_size"] == 1
    assert "deck" not in view["players"]["A"]
    assert "deck" not in view["players"]["B"]


def test_observable_exposes_public_zones_and_match_state() -> None:
    view = _populated_state().observable("A")
    for key in ("A", "B"):
        seat = view["players"][key]
        assert [c["db_uuid"] for c in seat["discard"]]  # discard piles public
        assert [c["db_uuid"] for c in seat["in_play"]]  # boards public
        assert "competitor" in seat and "entrance" in seat
        assert seat["gimmick_blanked"] is False
    assert (view["crowd_meter"], view["active"], view["turn_no"]) == (2, "B", 4)


def test_observable_omits_engine_bookkeeping() -> None:
    view = _populated_state().observable("B")
    for key in ("A", "B"):
        seat = view["players"][key]
        for hidden in ("flags", "freq_counters", "pending_roll_mods"):
            assert hidden not in seat
    assert "rng" not in view


def test_observable_is_symmetric_from_each_seat() -> None:
    gs = _populated_state()
    a_view, b_view = gs.observable("A"), gs.observable("B")
    # Each seat sees its own hand, never the other's.
    assert "hand" in a_view["players"]["A"] and "hand_size" in a_view["players"]["B"]
    assert "hand" in b_view["players"]["B"] and "hand_size" in b_view["players"]["A"]


def test_active_peek_reveals_opponent_hand_this_turn_only() -> None:
    gs = _populated_state()  # turn_no == 4
    gs.players["A"].flags["peek"] = {"B": 4}
    revealed = gs.observable("A")["players"]["B"]
    assert [c["db_uuid"] for c in revealed["hand"]] == ["u-11", "u-12", "u-13"]
    assert "hand_size" not in revealed  # full contents, not the count
    # The peek is scoped to the turn it was taken: a later turn redacts again.
    gs.turn_no = 5
    later = gs.observable("A")["players"]["B"]
    assert "hand" not in later and later["hand_size"] == 3


def test_peek_is_one_directional_and_never_self() -> None:
    gs = _populated_state()
    gs.players["A"].flags["peek"] = {"B": 4}
    # B did not peek A, so B's view of A stays redacted.
    assert "hand" not in gs.observable("B")["players"]["A"]
    # A never "peeks" its own already-visible hand.
    assert gs._peeked("A", "A") is False
