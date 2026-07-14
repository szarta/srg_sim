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
) -> GameState:
    ent = EntranceCard("u-ent", "Entrance")
    bull = Competitor("u-bull", "The Bull", "Worlds", BULL_STATS, effects=a_comp_effects)
    fae = Competitor("u-fae", "Fae Dragon", "Worlds", FAE_STATS)
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
