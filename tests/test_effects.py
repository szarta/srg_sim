"""Round-trip + invariant tests for the Effect IR (DESIGN.md §3)."""

from __future__ import annotations

from dataclasses import FrozenInstanceError

import pytest
from srg_sim.cards import AtkType, PlayOrder, Skill
from srg_sim.effects import (
    _REGISTRY,
    AddFromDiscard,
    AlsoLead,
    Always,
    And,
    BlankGimmick,
    BlankText,
    BreakoutModifier,
    BuffSkill,
    Bump,
    Bury,
    BuryFrom,
    CardFilter,
    Choice,
    ChoiceOption,
    Comparator,
    CountsAsInPlay,
    CrowdMeter,
    CrowdMeterCompare,
    DeckEnd,
    Dest,
    Direction,
    Discard,
    DoubleFinishIfBumped,
    Draw,
    Effect,
    EffectSource,
    ElectBumpOnSameSkill,
    FinishBonus,
    FinishRollBonus,
    Flip,
    FlipGimmick,
    FlipGimmickSigns,
    Frequency,
    FrequencyGuard,
    GimmickFlipped,
    HandSizeCompare,
    HasInDiscard,
    HasInHand,
    HasInPlay,
    InRoll,
    IRNode,
    DisqualificationRule,
    DqScope,
    LoseBy,
    LoseKind,
    LowestRollWins,
    MaxHandSize,
    ModifyRoll,
    Not,
    OnBreakout,
    OnBump,
    OnHit,
    OnLoseTurn,
    OnPlay,
    OnRoll,
    OnRollBoost,
    OnStop,
    OnWinTurn,
    OppWonLastRoll,
    Or,
    Peek,
    PlayExtraCard,
    RecurToDeckTop,
    RemoveFromPlay,
    Reroll,
    RevealAndDiscard,
    RollGapAtLeast,
    RollGapExactly,
    RollLeadAtLeast,
    Scry,
    ScryRest,
    RollValue,
    RollWasSkill,
    RollWhen,
    Search,
    SetFinishRoll,
    ShuffleDeck,
    ShuffleIntoDeck,
    SkillCompare,
    StartOfMatch,
    StartOfTurn,
    Static,
    Stop,
    Unstoppable,
    Unsupported,
    Until,
    Vs,
    Who,
    WinTie,
    from_dict,
    from_json,
    to_json,
)

# One representative instance of every IR node type. The coverage test below
# asserts this list stays in lock-step with the registry, so a newly added node
# cannot slip through untested.
SAMPLES: list[IRNode] = [
    CardFilter(
        number=5, atk_type=AtkType.STRIKE, play_order=PlayOrder.LEAD, tag="t", name="n", raw="r"
    ),
    # triggers
    OnPlay(),
    OnRoll(Skill.STRIKE, Who.OPP),
    InRoll(Skill.POWER, either=True),  # Tomato Tomato Jr. capped -1 roll debuff
    OnRollBoost(Skill.GRAPPLE, 1),
    OnRollBoost(None, 1, on_bump=True),  # Rey Zerblade would-bump replacement
    OnWinTurn(),
    OnLoseTurn(by=2),
    OnStop(Direction.YOURS),
    OnHit(name_contains=("Signature",)),  # gimmick "when you hit a card with X in the name"
    OnHit(atk_type=AtkType.SUBMISSION),  # gimmick "when you hit a Submission"
    OnBump(),
    OnBreakout(),
    StartOfTurn(),
    StartOfMatch(),
    Static(),
    # conditions
    Always(),
    And((SkillCompare(Skill.POWER, Comparator.GT), Not(Always()))),
    Or((Always(), CrowdMeterCompare(Comparator.EQ, 0))),
    Not(Always()),
    SkillCompare(Skill.POWER, Comparator.GT, Who.SELF, Vs.OPP_SAME, None),
    SkillCompare(  # cross-skill: "your Strike > opponent's Agility" (Thunderous Dropkick)
        Skill.STRIKE, Comparator.GT, Who.SELF, Vs.OPP_SAME, None, vs_skill=Skill.AGILITY
    ),
    HandSizeCompare(Comparator.GE, Vs.VALUE, 3, Who.OPP),
    CrowdMeterCompare(Comparator.LT, 4),
    HasInPlay(Who.SELF, CardFilter(tag="Championship")),
    HasInHand(Who.SELF, CardFilter(atk_type=AtkType.STRIKE)),
    HasInDiscard(Who.OPP),
    RollWasSkill(Skill.AGILITY),
    RollGapExactly(2),
    RollGapAtLeast(3),
    RollLeadAtLeast(3),
    OppWonLastRoll(),
    RollValue(Comparator.LE, 7),
    GimmickFlipped(Who.SELF),
    # actions
    Draw(2, DeckEnd.BOTTOM),
    Bury(CardFilter(number=1), 2),
    Bury(CardFilter(), 1, Who.OPP, False, BuryFrom.HAND),
    Flip(2, Who.OPP),
    Discard(CardFilter(play_order=PlayOrder.FINISH), 1),
    Search(CardFilter(name="Colossal Smash"), Dest.HAND),
    ShuffleDeck(Who.SELF),
    ShuffleIntoDeck(CardFilter(tag="x")),
    AddFromDiscard(CardFilter(atk_type=AtkType.SUBMISSION)),
    RemoveFromPlay(CardFilter(play_order=PlayOrder.FOLLOWUP), Who.OPP, 1),
    Peek(Who.OPP),
    Scry(deck=Who.SELF, top=2, to_hand=1, bury=1, reveal=True, rest=ScryRest.CHOOSE),
    RecurToDeckTop(CardFilter(play_order=PlayOrder.FINISH), 3),
    CountsAsInPlay(  # "counts as 2 Lead Strikes in play" (Double Cross)
        CardFilter(play_order=PlayOrder.LEAD, atk_type=AtkType.STRIKE), 2
    ),
    RevealAndDiscard(3, Who.OPP),  # Spin Wheel Kick reveal-3-discard-Stops
    ModifyRoll(Who.SELF, 1, RollWhen.NEXT),
    Draw(1, per=CardFilter(play_order=PlayOrder.LEAD), per_who=Who.SELF),  # per-count draw
    Discard(  # per-count opponent discard (Field of Fire)
        count=1, who=Who.OPP, per=CardFilter(atk_type=AtkType.STRIKE), per_who=Who.SELF
    ),
    BuffSkill(Skill.POWER, 1),
    BuffSkill(Skill.GRAPPLE, 0, Who.SELF, target_highest=True, per_crowd=True, cap=5),
    MaxHandSize(-1, Who.OPP),
    Reroll(Who.OPP, once=False),
    WinTie(Who.SELF),
    Bump(Who.OPP),
    ElectBumpOnSameSkill(uses=2),  # Ringside Ruckus entrance: elective same-skill bump
    Stop(PlayOrder.LEAD, AtkType.GRAPPLE, source_is_skillreq=True),
    BlankGimmick(Who.OPP),
    FlipGimmick(Who.SELF),
    FlipGimmickSigns(Who.OPP),
    BlankText(CardFilter(name="Gimmick"), Until.END_OF_TURN),
    LoseBy(LoseKind.PINFALL, Who.SELF),
    DisqualificationRule(enabled=False, scope=DqScope.MATCH),
    CrowdMeter(1),
    PlayExtraCard(PlayOrder.FINISH),
    SetFinishRoll(11, CrowdMeterCompare(Comparator.GT, 0)),
    FinishBonus(Skill.STRIKE, 2),
    FinishRollBonus(3),
    FinishRollBonus(1, when_skill=Skill.AGILITY, either=True),  # Spin Wheel Kick conditional
    BreakoutModifier(1, attempts=2),
    LowestRollWins(),
    Unstoppable(by_order=PlayOrder.FOLLOWUP),  # "Cannot be stopped by Follow Ups"
    AlsoLead(HandSizeCompare(Comparator.LE, Vs.VALUE, 1)),  # Broken Butterfly empty-hand Lead
    DoubleFinishIfBumped(),  # T-Virus "double these bonuses if you bumped"
    ChoiceOption("draw", (Draw(n=1),)),
    Choice(
        options=(
            ChoiceOption("draw 1", (Draw(n=1),)),
            ChoiceOption("opp next roll -2", (ModifyRoll(Who.OPP, -2, RollWhen.NEXT),)),
        )
    ),
    # sentinels / meta
    Unsupported("some weird clause", "no grammar match"),
    FrequencyGuard(Frequency.N_PER_MATCH, 2),
    Effect(trigger=OnPlay()),
]


@pytest.mark.parametrize("node", SAMPLES, ids=lambda n: type(n).__name__)
def test_dict_round_trip(node: IRNode) -> None:
    assert from_dict(node.to_dict()) == node


@pytest.mark.parametrize("node", SAMPLES, ids=lambda n: type(n).__name__)
def test_json_round_trip(node: IRNode) -> None:
    assert from_json(to_json(node)) == node


def test_samples_cover_every_registered_node() -> None:
    covered = {type(n).__name__ for n in SAMPLES}
    assert covered == set(_REGISTRY), (
        "SAMPLES and the node registry diverged: "
        f"missing={set(_REGISTRY) - covered} extra={covered - set(_REGISTRY)}"
    )


def test_to_dict_tags_type_and_stringifies_enums() -> None:
    d = BuffSkill(Skill.POWER, 1, Who.SELF).to_dict()
    assert d["@type"] == "BuffSkill"
    assert d["skill"] == "Power"  # enum serialized to its DB string value
    assert d["who"] == "SELF"


def test_field_named_kind_does_not_clobber_type_tag() -> None:
    # FrequencyGuard and LoseBy both have a field literally named ``kind``.
    for node in (FrequencyGuard(Frequency.ONCE_PER_MATCH), LoseBy(LoseKind.PINFALL)):
        d = node.to_dict()
        assert d["@type"] == type(node).__name__
        assert from_dict(d) == node


def test_effect_is_hashable_and_frozen() -> None:
    effect = Effect(
        trigger=OnRoll(Skill.STRIKE),
        actions=(BuffSkill(Skill.STRIKE, 1), Draw(1)),
        source=EffectSource.GIMMICK,
    )
    # frozen: fields cannot be reassigned
    with pytest.raises(FrozenInstanceError):
        effect.trigger = OnPlay()  # type: ignore[misc]
    # hashable: usable in a set, identical effects dedupe
    twin = Effect(
        trigger=OnRoll(Skill.STRIKE),
        actions=(BuffSkill(Skill.STRIKE, 1), Draw(1)),
        source=EffectSource.GIMMICK,
    )
    assert len({effect, twin}) == 1


def test_defaults_survive_round_trip() -> None:
    effect = Effect(trigger=OnPlay())
    restored = from_dict(effect.to_dict())
    assert restored == effect
    assert isinstance(restored, Effect)
    assert restored.condition == Always()
    assert restored.duration.value == "INSTANT"
    assert restored.source == EffectSource.CARD


def test_complex_nested_effect_round_trips() -> None:
    effect = Effect(
        trigger=OnRoll(Skill.SUBMISSION, Who.SELF),
        condition=And(
            (
                SkillCompare(Skill.SUBMISSION, Comparator.GT, Who.SELF, Vs.OPP_SAME),
                Or(
                    (
                        CrowdMeterCompare(Comparator.GE, 3),
                        HasInPlay(Who.SELF, CardFilter(tag="Finish")),
                    )
                ),
                Not(RollGapAtLeast(2)),
            )
        ),
        actions=(
            BuffSkill(Skill.SUBMISSION, 2, Who.SELF),
            Stop(atk_type=AtkType.STRIKE),
            SetFinishRoll(11, CrowdMeterCompare(Comparator.GT, 0)),
            Unsupported("residual clause", "partial parse"),
        ),
        frequency=FrequencyGuard(Frequency.ONCE_PER_TURN),
        raw_clause="When you roll Submission and your Submission beats theirs...",
        source=EffectSource.CARD,
    )
    assert from_json(to_json(effect)) == effect


def test_modify_roll_per_count_round_trips() -> None:
    m = ModifyRoll(Who.SELF, 1, RollWhen.NEXT, CardFilter(play_order=PlayOrder.LEAD), Who.OPP)
    assert from_dict(m.to_dict()) == m
    assert ModifyRoll(Who.SELF, 1).per is None  # default: a plain fixed-delta roll mod


def test_optional_flag_round_trips_and_defaults_false() -> None:
    assert Effect(trigger=OnHit()).optional is False  # default
    opt = Effect(trigger=OnHit(), actions=(Flip(1, Who.OPP),), optional=True)
    assert from_dict(opt.to_dict()) == opt
    assert from_dict(opt.to_dict()).optional is True


def test_search_to_discard_round_trips_and_defaults() -> None:
    assert Search().dest is Dest.HAND and Search().count == 1  # defaults unchanged
    s = Search(CardFilter(), Dest.DISCARD, count=4)  # #49 mill-to-discard
    assert from_dict(s.to_dict()) == s
    assert s.to_dict()["dest"] == "DISCARD"


def test_unknown_type_raises() -> None:
    with pytest.raises(KeyError):
        from_dict({"@type": "NoSuchNode"})


def test_flip_signs_negates_printed_deltas_but_not_counts() -> None:
    from srg_sim.effects import flip_signs

    effect = Effect(
        trigger=OnRoll(),
        actions=(
            ModifyRoll(Who.OPP, -2, RollWhen.NEXT),  # printed sign -> flips
            BuffSkill(Skill.POWER, 3),  # +3 to Power -> flips
            Draw(2),  # a count, no sign -> untouched
            Choice(
                options=(
                    ChoiceOption("buff", (BuffSkill(Skill.STRIKE, 1),)),  # nested -> flips
                    ChoiceOption("draw", (Draw(1),)),  # nested count -> untouched
                )
            ),
        ),
    )
    flipped = flip_signs(effect)
    assert flipped.actions[0].delta == 2  # -2 -> +2
    assert flipped.actions[1].delta == -3  # +3 -> -3
    assert flipped.actions[2].n == 2  # Draw count unchanged
    choice = flipped.actions[3]
    assert choice.options[0].actions[0].delta == -1  # nested BuffSkill flipped
    assert choice.options[1].actions[0].n == 1  # nested Draw unchanged
