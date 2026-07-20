"""Tests for the Effect-IR condition evaluator (DESIGN.md §3)."""

from __future__ import annotations

from srg_sim import effects as fx
from srg_sim.cards import AtkType, Card, Competitor, EntranceCard, PlayOrder, Skill, Stats
from srg_sim.conditions import RollContext, card_matches, holds
from srg_sim.rng import SeededRNG
from srg_sim.state import GameState, PlayerState

# A: higher Strike (7) than B (5); B has higher Submission.
A_STATS = Stats(power=10, technique=6, agility=5, submission=8, grapple=9, strike=7)
B_STATS = Stats(power=10, technique=7, agility=6, submission=9, grapple=5, strike=5)


def _card(number: int, atk: AtkType = AtkType.STRIKE, order: PlayOrder = PlayOrder.LEAD) -> Card:
    return Card(
        db_uuid=f"u{number}", name=f"C{number}", number=number, atk_type=atk, play_order=order
    )


def _state() -> GameState:
    ent = EntranceCard("e", "Ent")
    a = PlayerState(competitor=Competitor("cA", "A", "W", A_STATS), entrance=ent)
    b = PlayerState(competitor=Competitor("cB", "B", "W", B_STATS), entrance=ent)
    return GameState(players={"A": a, "B": b}, rng=SeededRNG(1))


# --- boolean combinators ---------------------------------------------------


def test_always() -> None:
    assert holds(fx.Always(), _state(), "A")


def test_and_or_not() -> None:
    s = _state()
    yes = fx.CrowdMeterCompare(fx.Comparator.EQ, 0)
    no = fx.CrowdMeterCompare(fx.Comparator.GT, 0)
    assert holds(fx.And(items=(yes, yes)), s, "A")
    assert not holds(fx.And(items=(yes, no)), s, "A")
    assert holds(fx.Or(items=(yes, no)), s, "A")
    assert holds(fx.Not(item=no), s, "A")


# --- skill compare (the skill-stop / buff predicate) -----------------------


def test_skill_compare_vs_opponent_same() -> None:
    s = _state()
    # A Strike 7 > B Strike 5 -> online for A; the reverse is offline for B.
    online = fx.SkillCompare(Skill.STRIKE, fx.Comparator.GT, fx.Who.SELF, fx.Vs.OPP_SAME)
    assert holds(online, s, "A")
    assert not holds(online, s, "B")


def test_skill_compare_is_strict() -> None:
    s = _state()
    s.players["B"].competitor = s.players["A"].competitor  # equal Strike both sides
    online = fx.SkillCompare(Skill.STRIKE, fx.Comparator.GT, fx.Who.SELF, fx.Vs.OPP_SAME)
    assert not holds(online, s, "A")  # a tie does not bring it online


def test_skill_compare_vs_value() -> None:
    s = _state()
    cond = fx.SkillCompare(Skill.POWER, fx.Comparator.GE, fx.Who.SELF, fx.Vs.VALUE, value=10)
    assert holds(cond, s, "A")


def test_skill_compare_cross_skill_vs_a_different_opponent_skill() -> None:
    # "your Strike > opponent's Agility" (Thunderous Dropkick): A Strike 7 vs B Agility 6.
    s = _state()
    beats = fx.SkillCompare(
        Skill.STRIKE, fx.Comparator.GT, fx.Who.SELF, fx.Vs.OPP_SAME, vs_skill=Skill.AGILITY
    )
    assert holds(beats, s, "A")  # 7 > 6
    loses = fx.SkillCompare(
        Skill.STRIKE, fx.Comparator.GT, fx.Who.SELF, fx.Vs.OPP_SAME, vs_skill=Skill.SUBMISSION
    )
    assert not holds(loses, s, "A")  # A Strike 7 is not > B Submission 9


def test_skill_compare_reflects_active_buffs() -> None:
    # A card buffing A's Strike can flip a Strike-keyed stop online (mechanics §6).
    s = _state()
    buff = fx.Effect(
        trigger=fx.Static(),
        actions=(fx.BuffSkill(Skill.GRAPPLE, 3, fx.Who.SELF, fx.Duration.WHILE_IN_PLAY),),
        duration=fx.Duration.WHILE_IN_PLAY,
    )
    s.players["A"].in_play.append(
        Card(
            db_uuid="b",
            name="Buff",
            number=1,
            atk_type=AtkType.STRIKE,
            play_order=PlayOrder.LEAD,
            effects=(buff,),
        )
    )
    # A Grapple 9 +3 = 12 vs B Grapple 5.
    cond = fx.SkillCompare(Skill.GRAPPLE, fx.Comparator.GT, fx.Who.SELF, fx.Vs.OPP_SAME)
    assert holds(cond, s, "A")


# --- considered-compare meta-override (task #104) --------------------------


def _state_with_considered(domain: fx.CompareDomain, order: fx.CompareOrder) -> GameState:
    """`_state()` but A's competitor declares a `ConsideredCompare` override."""
    decl = fx.Effect(
        trigger=fx.Static(),
        actions=(fx.ConsideredCompare(domain=domain, order=order),),
        duration=fx.Duration.WHILE_IN_PLAY,
    )
    s = _state()
    comp = s.players["A"].competitor
    s.players["A"].competitor = Competitor("cA", "A", "W", A_STATS, effects=(decl,))
    assert comp is not s.players["A"].competitor
    return s


def test_considered_skill_greater_is_strict() -> None:
    # A.Submission 8 < B.Submission 9: naturally `<` True, `>`/`=` False.
    s = _state()
    lt = fx.SkillCompare(Skill.SUBMISSION, fx.Comparator.LT, fx.Who.SELF, fx.Vs.OPP_SAME)
    assert holds(lt, s, "A")
    # RaRa Perre: A's skills considered GREATER — `>`/`>=` hold, `<`/`<=`/`=` fail.
    g = _state_with_considered(fx.CompareDomain.SKILL, fx.CompareOrder.GREATER)
    gt = fx.SkillCompare(Skill.SUBMISSION, fx.Comparator.GT, fx.Who.SELF, fx.Vs.OPP_SAME)
    eq = fx.SkillCompare(Skill.SUBMISSION, fx.Comparator.EQ, fx.Who.SELF, fx.Vs.OPP_SAME)
    assert holds(gt, g, "A")
    assert not holds(lt, g, "A")
    assert not holds(eq, g, "A")
    # Scoped to A: a B-owned compare (subject=B) is unaffected (B.Sub 9 > A.Sub 8).
    assert holds(fx.SkillCompare(Skill.SUBMISSION, fx.Comparator.GT, fx.Who.SELF, fx.Vs.OPP_SAME), g, "B")


def test_considered_hand_less_is_strict() -> None:
    # Theo V2: A's hand considered fewer — `<`/`<=` hold, `>`/`=` fail, any sizes.
    l = _state_with_considered(fx.CompareDomain.HAND, fx.CompareOrder.LESS)
    l.players["A"].hand = [_card(1), _card(2), _card(3)]  # A has MORE, yet…
    l.players["B"].hand = [_card(1)]
    lt = fx.HandSizeCompare(fx.Comparator.LT, fx.Vs.OPP, who=fx.Who.SELF)
    gt = fx.HandSizeCompare(fx.Comparator.GT, fx.Vs.OPP, who=fx.Who.SELF)
    assert holds(lt, l, "A")  # forced fewer despite the larger hand
    assert not holds(gt, l, "A")
    # A SKILL override must not touch a HandSizeCompare.
    g = _state_with_considered(fx.CompareDomain.SKILL, fx.CompareOrder.GREATER)
    g.players["A"].hand = [_card(1)]
    g.players["B"].hand = [_card(1), _card(2)]
    assert holds(fx.HandSizeCompare(fx.Comparator.LT, fx.Vs.OPP, who=fx.Who.SELF), g, "A")  # real 1<2


# --- hand size / crowd meter -----------------------------------------------


def test_hand_size_vs_opponent() -> None:
    s = _state()
    s.players["A"].hand = [_card(1), _card(2), _card(3)]
    s.players["B"].hand = [_card(1)]
    more = fx.HandSizeCompare(fx.Comparator.GT, fx.Vs.OPP, who=fx.Who.SELF)
    assert holds(more, s, "A")
    assert not holds(more, s, "B")


def test_during_turn_reads_active_player() -> None:
    # La Fenix: "during your opponent's turn". A's opponent is B.
    s = _state()
    s.active = "B"  # opponent's turn
    assert holds(fx.DuringTurn(fx.Who.OPP), s, "A")
    assert not holds(fx.DuringTurn(fx.Who.SELF), s, "A")
    s.active = "A"  # own turn
    assert not holds(fx.DuringTurn(fx.Who.OPP), s, "A")
    assert holds(fx.DuringTurn(fx.Who.SELF), s, "A")


def test_crowd_meter_compare() -> None:
    s = _state()
    s.crowd_meter = 3
    assert holds(fx.CrowdMeterCompare(fx.Comparator.GE, 2), s, "A")
    assert not holds(fx.CrowdMeterCompare(fx.Comparator.GE, 4), s, "A")


# --- has in play / discard (the see-1 predicate) ---------------------------


def test_has_in_play_by_type() -> None:
    s = _state()
    s.players["B"].in_play.append(_card(2, atk=AtkType.GRAPPLE))
    # "opponent has a Grapple in play" — the see-1 gate, evaluated from A's view.
    cond = fx.HasInPlay(fx.Who.OPP, fx.CardFilter(atk_type=AtkType.GRAPPLE))
    assert holds(cond, s, "A")
    assert not holds(fx.HasInPlay(fx.Who.OPP, fx.CardFilter(atk_type=AtkType.STRIKE)), s, "A")


def test_has_in_play_count_gated() -> None:
    s = _state()
    # "opponent has 2 other Grapples in play" -> a >=2 count gate from A's view.
    cond = fx.HasInPlay(
        fx.Who.OPP, fx.CardFilter(atk_type=AtkType.GRAPPLE), count=2, cmp=fx.Comparator.GE
    )
    assert not holds(cond, s, "A")  # zero grapples
    s.players["B"].in_play.append(_card(2, atk=AtkType.GRAPPLE))
    assert not holds(cond, s, "A")  # one is not enough
    s.players["B"].in_play.append(_card(3, atk=AtkType.GRAPPLE))
    assert holds(cond, s, "A")  # two clears the gate
    # Non-matching cards do not count toward the tally.
    assert not holds(
        fx.HasInPlay(fx.Who.OPP, fx.CardFilter(atk_type=AtkType.STRIKE), count=2), s, "A"
    )


def test_in_play_compare_target_has_more() -> None:
    s = _state()
    strike = fx.CardFilter(atk_type=AtkType.STRIKE)
    # Snake Pitt V3: "when your target (OPP) has more Strikes in play than you (SELF)".
    more = fx.InPlayCompare(strike, fx.Comparator.GT, fx.Who.OPP, fx.Who.SELF)
    s.players["A"].in_play.append(_card(1, atk=AtkType.STRIKE))
    s.players["B"].in_play.append(_card(2, atk=AtkType.STRIKE))
    assert not holds(more, s, "A")  # 1 == 1, not "more"
    s.players["B"].in_play.append(_card(3, atk=AtkType.STRIKE))
    assert holds(more, s, "A")  # 2 > 1
    # Direction matters: from A's view "you have more" (SELF > OPP) is false here.
    assert not holds(
        fx.InPlayCompare(strike, fx.Comparator.GT, fx.Who.SELF, fx.Who.OPP), s, "A"
    )
    # Non-matching cards are not counted: Grapples do not tip the Strike compare.
    s.players["A"].in_play.append(_card(4, atk=AtkType.GRAPPLE))
    assert holds(more, s, "A")


def test_has_in_discard() -> None:
    s = _state()
    s.players["A"].discard.append(_card(28, order=PlayOrder.FINISH))
    cond = fx.HasInDiscard(fx.Who.SELF, fx.CardFilter(play_order=PlayOrder.FINISH))
    assert holds(cond, s, "A")


# --- roll-scoped conditions ------------------------------------------------


def test_roll_conditions_need_context() -> None:
    s = _state()
    was = fx.RollWasSkill(Skill.POWER)
    assert not holds(was, s, "A")  # no roll context -> false
    assert holds(was, s, "A", RollContext(skill=Skill.POWER))
    assert holds(fx.RollGapAtLeast(2), s, "A", RollContext(gap=3))
    assert not holds(fx.RollGapAtLeast(2), s, "A", RollContext(gap=1))
    assert holds(fx.RollGapExactly(3), s, "A", RollContext(gap=3))


def test_printed_roll_value_reads_the_printed_stat() -> None:
    # Collin the Chrononaut: "when your opponent rolls their printed 8 skill".
    # B's printed Power is 10, Strike 5 (A_STATS/B_STATS from the module head).
    s = _state()
    printed10 = fx.PrintedRollValue(fx.Who.OPP, 10)  # owner A reads B's printed stat
    # B rolls Power (printed 10) -> True; the ROLLED VALUE is irrelevant.
    assert holds(printed10, s, "A", RollContext(skill=Skill.POWER, value=3))
    # B rolls Strike (printed 5) -> False even though the rolled value is 10.
    assert not holds(printed10, s, "A", RollContext(skill=Skill.STRIKE, value=10))
    # No roll context -> False; and who=SELF reads A's own printed stat (Power 10).
    assert not holds(printed10, s, "A", None)
    assert holds(fx.PrintedRollValue(fx.Who.SELF, 10), s, "A", RollContext(skill=Skill.POWER))


def test_same_rolled_skill_compares_both_sides() -> None:
    # Hex / Nic Nemeth: "you and your target roll the same skill".
    s = _state()
    c = fx.SameRolledSkill()
    assert holds(c, s, "A", RollContext(skill=Skill.POWER, opp_skill=Skill.POWER))
    assert not holds(c, s, "A", RollContext(skill=Skill.POWER, opp_skill=Skill.STRIKE))
    # A single-sided context (opp_skill unset — a re-roll/switch ctx) and no context
    # both read False.
    assert not holds(c, s, "A", RollContext(skill=Skill.POWER))
    assert not holds(c, s, "A", None)


# --- card filter -----------------------------------------------------------


def test_card_matches_all_criteria_and() -> None:
    card = _card(4, atk=AtkType.GRAPPLE, order=PlayOrder.LEAD)
    assert card_matches(card, fx.CardFilter(atk_type=AtkType.GRAPPLE, play_order=PlayOrder.LEAD))
    assert not card_matches(card, fx.CardFilter(atk_type=AtkType.STRIKE))
    assert card_matches(card, fx.CardFilter())  # empty filter matches anything
