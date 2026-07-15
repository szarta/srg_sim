"""Round-trip + invariant tests for the Effect IR (DESIGN.md §3)."""

from __future__ import annotations

from dataclasses import FrozenInstanceError

import pytest
from srg_sim.cards import AtkType, PlayOrder, Skill
from srg_sim.effects import (
    _REGISTRY,
    AddFromDiscard,
    Always,
    And,
    BlankGimmick,
    BlankText,
    BreakoutModifier,
    BuffSkill,
    Bump,
    Bury,
    CardFilter,
    Comparator,
    CrowdMeter,
    CrowdMeterCompare,
    DeckEnd,
    Dest,
    Direction,
    Discard,
    Draw,
    Effect,
    EffectSource,
    FinishBonus,
    FinishRollBonus,
    Flip,
    Frequency,
    FrequencyGuard,
    HandSizeCompare,
    HasInDiscard,
    HasInPlay,
    IRNode,
    LoseBy,
    LoseKind,
    ModifyRoll,
    Not,
    OnHit,
    OnLoseTurn,
    OnPlay,
    OnRoll,
    OnStop,
    OnWinTurn,
    Or,
    PlayExtraCard,
    Reroll,
    RollGapAtLeast,
    RollGapExactly,
    RollWasSkill,
    RollWhen,
    Search,
    SetFinishRoll,
    ShuffleIntoDeck,
    SkillCompare,
    StartOfMatch,
    StartOfTurn,
    Static,
    Stop,
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
    OnWinTurn(),
    OnLoseTurn(by=2),
    OnStop(Direction.YOURS),
    OnHit(keyword="Signature", name=None),
    StartOfTurn(),
    StartOfMatch(),
    Static(),
    # conditions
    Always(),
    And((SkillCompare(Skill.POWER, Comparator.GT), Not(Always()))),
    Or((Always(), CrowdMeterCompare(Comparator.EQ, 0))),
    Not(Always()),
    SkillCompare(Skill.POWER, Comparator.GT, Who.SELF, Vs.OPP_SAME, None),
    HandSizeCompare(Comparator.GE, Vs.VALUE, 3, Who.OPP),
    CrowdMeterCompare(Comparator.LT, 4),
    HasInPlay(Who.SELF, CardFilter(tag="Championship")),
    HasInDiscard(Who.OPP),
    RollWasSkill(Skill.AGILITY),
    RollGapExactly(2),
    RollGapAtLeast(3),
    # actions
    Draw(2, DeckEnd.BOTTOM),
    Bury(CardFilter(number=1), 2),
    Flip(2),
    Discard(CardFilter(play_order=PlayOrder.FINISH), 1),
    Search(CardFilter(name="Colossal Smash"), Dest.HAND),
    ShuffleIntoDeck(CardFilter(tag="x")),
    AddFromDiscard(CardFilter(atk_type=AtkType.SUBMISSION)),
    ModifyRoll(Who.SELF, 1, RollWhen.NEXT),
    BuffSkill(Skill.POWER, 1),
    Reroll(Who.OPP, once=False),
    WinTie(Who.SELF),
    Bump(Who.OPP),
    Stop(PlayOrder.LEAD, AtkType.GRAPPLE, source_is_skillreq=True),
    BlankGimmick(Who.OPP),
    BlankText(CardFilter(name="Gimmick"), Until.END_OF_TURN),
    LoseBy(LoseKind.PINFALL, Who.SELF),
    CrowdMeter(1),
    PlayExtraCard(PlayOrder.FINISH),
    SetFinishRoll(11, CrowdMeterCompare(Comparator.GT, 0)),
    FinishBonus(Skill.STRIKE, 2),
    FinishRollBonus(3),
    BreakoutModifier(1, attempts=2),
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


def test_unknown_type_raises() -> None:
    with pytest.raises(KeyError):
        from_dict({"@type": "NoSuchNode"})
