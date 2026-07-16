"""Engine tests: turn loop, stops, finish, executor, determinism (§6)."""

from __future__ import annotations

import collections
import json
from dataclasses import replace

import pytest
from srg_sim import effects as fx
from srg_sim.cards import AtkType, Card, PlayOrder, Skill
from srg_sim.engine import Engine, GameResult, beats
from srg_sim.gamelog import GameLog
from srg_sim.policy import HeuristicPolicy, Policy, RandomPolicy

from tests.demo_decks import bull, bull_vs_fae, fae, make_deck, with_effects

VALID_REASONS = {"finish", "count_out", "disqualification", "pinfall", "turn_cap"}


def _play(seed: int, pa: Policy | None = None, pb: Policy | None = None) -> Engine:
    da, db = bull_vs_fae()
    eng = Engine(
        da, db, pa or RandomPolicy(), pb or RandomPolicy(), seed=seed, created="2026-07-14"
    )
    eng.play()
    return eng


# -- RPS + ordering primitives ----------------------------------------------


def test_rps_beats() -> None:
    assert beats(AtkType.GRAPPLE, AtkType.STRIKE)  # Strike-type stops a Grapple attack
    assert beats(AtkType.SUBMISSION, AtkType.GRAPPLE)
    assert beats(AtkType.STRIKE, AtkType.SUBMISSION)
    assert not beats(AtkType.STRIKE, AtkType.GRAPPLE)  # not symmetric
    assert not beats(AtkType.STRIKE, AtkType.NONE)


# -- a full game terminates with a valid, logged, replayable result ----------


@pytest.mark.parametrize("seed", range(8))
def test_game_reaches_valid_result(seed: int) -> None:
    eng = _play(seed)
    assert eng.result is not None
    assert eng.result.reason in VALID_REASONS
    assert eng.result.winner in {"A", "B", "draw"}
    assert eng.result.turns >= 1


@pytest.mark.parametrize("seed", range(8))
def test_determinism_same_seed_same_log(seed: int) -> None:
    assert _play(seed).state.log.to_lines() == _play(seed).state.log.to_lines()


def test_replay_matches_original() -> None:
    original = _play(11).state.log
    replayed = _play(11).state.log
    from srg_sim.gamelog import matches

    assert matches(original, replayed)


def test_log_round_trips_through_jsonl() -> None:
    lines = _play(4).state.log.to_lines()
    assert GameLog.parse(lines).to_lines() == lines


def test_last_event_is_result() -> None:
    lines = _play(7).state.log.to_lines()
    assert json.loads(lines[-1])["type"] == "result"


def test_header_records_policies_and_deck_refs() -> None:
    eng = _play(1, HeuristicPolicy(), RandomPolicy())
    header = eng.state.log.header
    assert header.players["A"].policy == "heuristic"
    assert header.players["B"].policy == "random"
    assert len(header.players["A"].deck) == 30


def test_heuristic_beats_or_ties_pure_random_over_seeds() -> None:
    # Not a strict guarantee, but the aggressive+defensive heuristic should not
    # lose badly to random over a fixed seed batch.
    wins = collections.Counter()
    for seed in range(40):
        da, db = bull_vs_fae()
        eng = Engine(da, db, HeuristicPolicy(), RandomPolicy(), seed=seed, created="x")
        wins[eng.play().winner] += 1
    assert wins["A"] >= wins["B"]


# -- both finish and count-out occur across seeds ----------------------------


def test_finishes_occur_across_seeds() -> None:
    reasons = collections.Counter(_play(s).result.reason for s in range(30))  # type: ignore[union-attr]
    assert reasons["finish"] > 0


def test_count_out_win_on_empty_deck_and_hand() -> None:
    # A player who must draw on a won turn with both deck and hand empty WINS.
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    eng.state.players["A"].deck.clear()
    eng.state.players["A"].hand.clear()
    assert eng._draw_for_turn("A") is False
    assert eng.result == GameResult("A", "count_out", eng.state.turn_no)


# -- decision logging policy -------------------------------------------------


def test_decision_events_have_multiple_legal_options() -> None:
    # _decide skips logging forced (single-option) choices, so every logged
    # decision reflects a real branch — the imitation-learning signal (§7).
    for line in _play(3).state.log.to_lines():
        ev = json.loads(line)
        if ev.get("type") == "decision":
            assert len(ev["legal"]) > 1


# -- effect executor ---------------------------------------------------------


def test_modify_roll_effect_emits_audit_and_shifts_a_roll() -> None:
    mod = fx.Effect(
        trigger=fx.OnWinTurn(),
        actions=(fx.ModifyRoll(who=fx.Who.SELF, delta=1, when=fx.RollWhen.NEXT),),
        raw_clause="+1 next roll",
        source=fx.EffectSource.GIMMICK,
    )
    eng = Engine(
        make_deck("A", with_effects(bull(), (mod,))),
        make_deck("B", fae()),
        HeuristicPolicy(),
        HeuristicPolicy(),
        seed=5,
        created="x",
    )
    eng.play()
    events = [json.loads(x) for x in eng.state.log.to_lines()[1:]]
    assert any(e["type"] == "effect" and e["action"] == "ModifyRoll" for e in events)
    assert any(e["type"] == "roll" and e["mods"] for e in events)


def test_draw_effect_logs_a_draw_not_an_effect_event() -> None:
    draw = fx.Effect(
        trigger=fx.OnWinTurn(),
        actions=(fx.Draw(n=1),),
        raw_clause="draw on win",
        source=fx.EffectSource.GIMMICK,
    )
    eng = Engine(
        make_deck("A", with_effects(bull(), (draw,))),
        make_deck("B", fae()),
        HeuristicPolicy(),
        HeuristicPolicy(),
        seed=2,
        created="x",
    )
    eng.play()
    types = collections.Counter(json.loads(x)["type"] for x in eng.state.log.to_lines()[1:])
    assert types["draw"] > 0  # Draw is logged as its concrete event, not `effect`


def test_opponent_draw_action_draws_for_the_opponent() -> None:
    # Draw(who=OPP) moves cards to the OTHER player's hand (#27 "your opponent draws N").
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    before = len(eng.state.players["B"].hand)
    eng._act_draw(fx.Draw(n=2, who=fx.Who.OPP), "A")
    assert len(eng.state.players["B"].hand) == before + 2


def test_shuffle_deck_action_reorders_without_losing_cards() -> None:
    # ShuffleDeck permutes the deck in place (#27 "Shuffle your deck").
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=4, created="x")
    eng.setup()
    before = list(eng.state.players["A"].deck)
    eng._act_shuffle_deck(fx.ShuffleDeck(), "A")
    after = eng.state.players["A"].deck
    assert sorted(c.db_uuid for c in after) == sorted(c.db_uuid for c in before)  # same multiset
    assert len(after) == len(before)


def test_on_bump_trigger_penalizes_the_opponents_next_roll() -> None:
    # Mastermind's gimmick: OnBump -> the opponent's NEXT turn roll is -2.
    gimmick = fx.Effect(
        trigger=fx.OnBump(),
        actions=(fx.ModifyRoll(who=fx.Who.OPP, delta=-2, when=fx.RollWhen.NEXT),),
        frequency=fx.FrequencyGuard(kind=fx.Frequency.ONCE_PER_TURN),
        raw_clause="bump -> opp next roll -2",
        source=fx.EffectSource.GIMMICK,
    )
    eng = Engine(
        make_deck("A", with_effects(bull(), (gimmick,))),
        make_deck("B", fae()),
        HeuristicPolicy(),
        HeuristicPolicy(),
        seed=1,
        created="x",
    )
    eng.setup()
    eng._run_on_bump()
    assert eng.state.players["B"].pending_roll_mods["next"] == -2  # opponent penalized
    assert eng.state.players["A"].pending_roll_mods["next"] == 0  # self untouched


def test_on_bump_gimmick_fires_only_once_per_turn() -> None:
    # A once-per-turn guard means repeated bumps in one turn punish only once.
    gimmick = fx.Effect(
        trigger=fx.OnBump(),
        actions=(fx.ModifyRoll(who=fx.Who.OPP, delta=-2, when=fx.RollWhen.NEXT),),
        frequency=fx.FrequencyGuard(kind=fx.Frequency.ONCE_PER_TURN),
        raw_clause="bump -> opp next roll -2",
        source=fx.EffectSource.GIMMICK,
    )
    eng = Engine(
        make_deck("A", with_effects(bull(), (gimmick,))),
        make_deck("B", fae()),
        HeuristicPolicy(),
        HeuristicPolicy(),
        seed=1,
        created="x",
    )
    eng.setup()
    eng._run_on_bump()
    eng._run_on_bump()  # a second bump the same turn
    assert eng.state.players["B"].pending_roll_mods["next"] == -2  # still only -2


def test_blank_gimmick_suppresses_opponent_competitor_gimmick_and_clears() -> None:
    # #47: a WHILE_IN_PLAY BlankGimmick on an in-play card drops the OPPONENT's
    # competitor gimmick out of their standing effects (derived, so it clears when
    # the blanking card leaves play on breakout) — the Savor-the-Moment counter.
    gim = fx.Effect(
        trigger=fx.OnRoll(),
        actions=(fx.Draw(n=2),),
        raw_clause="draw 2 on roll",
        source=fx.EffectSource.GIMMICK,
    )
    da = make_deck("A", with_effects(bull(), (gim,)))
    eng = Engine(
        da, make_deck("B", fae()), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x"
    )
    eng.setup()
    assert gim in eng._standing_effects("A") and not eng.state.is_gimmick_blanked("A")

    blanker = replace(
        _attack(AtkType.STRIKE, PlayOrder.LEAD),
        db_uuid="savor",
        effects=(fx.Effect(trigger=fx.Static(), actions=(fx.BlankGimmick(who=fx.Who.OPP),)),),
    )
    eng.state.players["B"].in_play.append(blanker)  # B blanks A (its opponent)
    assert eng.state.is_gimmick_blanked("A")
    assert gim not in eng._standing_effects("A")  # gimmick suppressed while blanked

    eng.state.players["B"].in_play.remove(blanker)  # source leaves play
    assert not eng.state.is_gimmick_blanked("A")  # blank clears


def test_conditional_blank_gimmick_honors_its_condition() -> None:
    # Savor the Moment: the opponent's gimmick is blank ONLY while "Enjoy Everything"
    # is in play — is_gimmick_blanked must evaluate the effect's condition, not just
    # its presence. Clears when the enabling card leaves play.
    enjoy = Card(
        db_uuid="enjoy",
        name="Enjoy Everything",
        number=10,
        atk_type=AtkType.STRIKE,
        play_order=PlayOrder.LEAD,
    )
    savor = Card(
        db_uuid="savor",
        name="Savor",
        number=16,
        atk_type=AtkType.STRIKE,
        play_order=PlayOrder.FOLLOWUP,
        effects=(
            fx.Effect(
                trigger=fx.Static(),
                condition=fx.HasInPlay(
                    who=fx.Who.SELF, filter=fx.CardFilter(name="Enjoy Everything")
                ),
                actions=(fx.BlankGimmick(who=fx.Who.OPP),),
            ),
        ),
    )
    eng = _fresh()
    a = eng.state.players["A"]
    a.in_play.append(savor)
    assert not eng.state.is_gimmick_blanked("B")  # Enjoy Everything not in play yet
    a.in_play.append(enjoy)
    assert eng.state.is_gimmick_blanked("B")  # condition now holds -> blanked
    a.in_play.remove(enjoy)
    assert not eng.state.is_gimmick_blanked("B")  # condition no longer holds


def test_modify_roll_per_count_scales_with_matching_cards() -> None:
    # Enjoy Everything: next turn roll +1 for EACH Lead the opponent has in play.
    eng = _fresh()
    eng.state.players["B"].in_play = [
        _attack(AtkType.STRIKE, PlayOrder.LEAD),
        _attack(AtkType.GRAPPLE, PlayOrder.LEAD),
        _attack(AtkType.STRIKE, PlayOrder.FOLLOWUP),  # not a Lead -> not counted
    ]
    eng._act_modify_roll(
        fx.ModifyRoll(
            who=fx.Who.SELF,
            delta=1,
            when=fx.RollWhen.NEXT,
            per=fx.CardFilter(play_order=PlayOrder.LEAD),
            per_who=fx.Who.OPP,
        ),
        "A",
    )
    assert eng.state.players["A"].pending_roll_mods["next"] == 2  # two opponent Leads -> +2


def test_blank_gimmick_action_latches_the_stored_flag() -> None:
    # A one-shot/executed BlankGimmick (not the Static/derived path) latches the flag.
    eng = _fresh()
    eng._act_blank_gimmick(fx.BlankGimmick(who=fx.Who.OPP), "A")  # A blanks B
    assert eng.state.players["B"].gimmick_blanked and eng.state.is_gimmick_blanked("B")


def test_search_tutors_a_matching_card_from_deck_to_hand() -> None:
    # Search pulls the first deck card matching the filter into hand and shuffles.
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    a = eng.state.players["A"]
    want = next(c for c in a.deck if c.play_order is PlayOrder.FINISH)
    hand_before = len(a.hand)
    eng._act_search(fx.Search(filter=fx.CardFilter(play_order=PlayOrder.FINISH)), "A")
    assert want in a.hand and want not in a.deck
    assert len(a.hand) == hand_before + 1


def test_search_with_no_match_only_shuffles() -> None:
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    a = eng.state.players["A"]
    a.deck = [c for c in a.deck if c.number != 99]  # (no card #99 exists)
    hand_before, deck_len = len(a.hand), len(a.deck)
    eng._act_search(fx.Search(filter=fx.CardFilter(number=99)), "A")
    assert len(a.hand) == hand_before and len(a.deck) == deck_len  # nothing tutored


def test_search_to_discard_bins_owner_chosen_cards_and_logs_them_public() -> None:
    # "Search your deck for up to N cards and put them into your discard" (#49,
    # Dest.DISCARD): the owner chooses which/how many; a trailing "none" stops early.
    # The binned cards land in the (public) discard, so the move is logged openly.
    from srg_sim import gamelog as gl

    class BinTwoThenStop(Policy):
        def __init__(self) -> None:
            super().__init__("bin-two")
            self.taken = 0

        def choose(self, point, legal, state, key):  # type: ignore[no-untyped-def]
            assert point == "search"  # routed to the owner, per card
            if self.taken < 2 and legal[0]["kind"] != "none":
                self.taken += 1
                return legal[0]
            return next(o for o in legal if o["kind"] == "none")  # decline the rest

    eng = Engine(*bull_vs_fae(), BinTwoThenStop(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    a = eng.state.players["A"]
    deck_before = len(a.deck)
    eng._act_search(fx.Search(dest=fx.Dest.DISCARD, count=4), "A")  # up to 4, take 2
    assert len(a.discard) == 2
    assert len(a.deck) == deck_before - 2
    assert all(c not in a.deck for c in a.discard)
    binned = [e for e in eng.state.log.events if isinstance(e, gl.Discard) and e.source == "deck"]
    assert len(binned) == 2 and all(e.hidden is False for e in binned)  # public in discard


def test_search_to_discard_caps_at_count() -> None:
    # A greedy owner (always bins the first offered) is still capped at `count`.
    class BinGreedy(Policy):
        def __init__(self) -> None:
            super().__init__("greedy")

        def choose(self, point, legal, state, key):  # type: ignore[no-untyped-def]
            return legal[0]  # never reaches the trailing "none"

    eng = Engine(*bull_vs_fae(), BinGreedy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    a = eng.state.players["A"]
    eng._act_search(fx.Search(dest=fx.Dest.DISCARD, count=3), "A")
    assert len(a.discard) == 3  # capped, not the whole deck


def test_add_from_discard_recurs_a_matching_card_to_hand() -> None:
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    a = eng.state.players["A"]
    card = next(c for c in a.deck if c.atk_type is AtkType.GRAPPLE)
    a.deck.remove(card)
    a.discard.append(card)
    eng._act_add_from_discard(
        fx.AddFromDiscard(filter=fx.CardFilter(atk_type=AtkType.GRAPPLE)), "A"
    )
    assert card in a.hand and card not in a.discard


def test_add_from_discard_lets_the_owner_choose_which_match() -> None:
    # Recursion is a player choice (DESIGN.md §7): with >1 match the owner picks via
    # the "target" decision point, not the engine auto-taking the first.
    class PickHighest(Policy):
        def __init__(self) -> None:
            super().__init__("pick-highest")

        def choose(self, point, legal, state, key):  # type: ignore[no-untyped-def]
            assert point == "target"  # engine routed the recur choice to the owner
            return max(legal, key=lambda o: o["number"])

    eng = Engine(*bull_vs_fae(), PickHighest(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    a = eng.state.players["A"]
    subs = [c for c in a.deck if c.atk_type is AtkType.SUBMISSION][:3]
    for c in subs:
        a.deck.remove(c)
        a.discard.append(c)
    want = max(subs, key=lambda c: c.number)
    eng._act_add_from_discard(
        fx.AddFromDiscard(filter=fx.CardFilter(atk_type=AtkType.SUBMISSION)), "A"
    )
    assert want in a.hand and want not in a.discard  # the chosen match, not the first


def test_shuffle_into_deck_recurs_one_card_from_discard_to_deck() -> None:
    # ShuffleIntoDeck moves ONE matching discard card back into the deck; "2 cards"
    # is authored as two actions, so two calls move two.
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    a = eng.state.players["A"]
    subs = [c for c in a.deck if c.atk_type is AtkType.SUBMISSION][:2]
    for c in subs:
        a.deck.remove(c)
        a.discard.append(c)
    deck_before = len(a.deck)
    sel = fx.ShuffleIntoDeck(selector=fx.CardFilter(atk_type=AtkType.SUBMISSION))
    eng._act_shuffle_into_deck(sel, "A")
    eng._act_shuffle_into_deck(sel, "A")
    assert all(c in a.deck for c in subs)
    assert all(c not in a.discard for c in subs)
    assert len(a.deck) == deck_before + 2


def test_recur_to_deck_top_puts_chosen_discards_on_top_of_deck() -> None:
    # #45 Chug-Chug: "up to 3 Finishes from discard on top of deck". The default
    # policy takes matches until they run out; the recurred card lands on TOP.
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    a = eng.state.players["A"]
    fins = [c for c in a.deck if c.play_order is PlayOrder.FINISH][:2]
    for c in fins:
        a.deck.remove(c)
        a.discard.append(c)
    eng._act_recur_to_deck_top(
        fx.RecurToDeckTop(selector=fx.CardFilter(play_order=PlayOrder.FINISH), count=3), "A"
    )
    assert all(c not in a.discard for c in fins)  # both recurred (fewer than the cap)
    assert a.deck[0] in fins and a.deck[1] in fins  # placed on top, ready to redraw


def test_recur_to_deck_top_owner_can_stop_early() -> None:
    class DeclineTarget(Policy):
        def __init__(self) -> None:
            super().__init__("decline")

        def choose(self, point, legal, state, key):  # type: ignore[no-untyped-def]
            return next((o for o in legal if o["kind"] == "none"), legal[0])  # stop at once

    eng = Engine(*bull_vs_fae(), DeclineTarget(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    a = eng.state.players["A"]
    fins = [c for c in a.deck if c.play_order is PlayOrder.FINISH][:2]
    for c in fins:
        a.deck.remove(c)
        a.discard.append(c)
    eng._act_recur_to_deck_top(
        fx.RecurToDeckTop(selector=fx.CardFilter(play_order=PlayOrder.FINISH), count=3), "A"
    )
    assert all(c in a.discard for c in fins)  # declined -> nothing recurred


def test_play_extra_card_grant_is_counted_and_consumed() -> None:
    # #45 Chug-Chug: PlayExtraCard banks a per-turn grant; _consume_extra_play spends it.
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    eng._act_play_extra_card(fx.PlayExtraCard(), "A")
    eng._act_play_extra_card(fx.PlayExtraCard(), "A")
    assert eng.state.players["A"].flags["extra_plays"] == 2
    assert eng._consume_extra_play("A") and eng._consume_extra_play("A")  # spends both
    assert eng._consume_extra_play("A") is False  # none left
    assert eng.state.players["A"].flags["extra_plays"] == 0


def test_turn_loop_runs_an_extra_action_when_granted() -> None:
    # The turn loop takes a second action when the first grants a PlayExtraCard.
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=3, created="x")
    eng.setup()
    seen: list[str] = []

    def fake(key: str) -> None:
        seen.append(key)
        if len(seen) == 1:
            eng._act_play_extra_card(fx.PlayExtraCard(), key)  # first action grants +1

    eng._take_turn_action = fake  # type: ignore[method-assign]
    eng._turn()
    assert len(seen) == 2  # base action + exactly one granted extra


def test_optional_effect_is_gated_and_can_flip_the_opponents_deck() -> None:
    # #45: a "you may" effect (Effect.optional) resolves only when the owner takes
    # it; Big Body Block's rider flips the OPPONENT's top card (Flip who=OPP).
    class Decide(Policy):
        def __init__(self, take: bool) -> None:
            super().__init__("decide")
            self.take = take

        def choose(self, point, legal, state, key):  # type: ignore[no-untyped-def]
            if point == "optional":
                return next(o for o in legal if o["kind"] == ("yes" if self.take else "no"))
            return legal[0]

    rider = fx.Effect(
        trigger=fx.OnHit(),
        actions=(fx.Flip(n=1, who=fx.Who.OPP),),
        optional=True,
        raw_clause="opp may flip their top card",
    )
    for take, delta in ((True, 1), (False, 0)):
        eng = Engine(*bull_vs_fae(), Decide(take), HeuristicPolicy(), seed=1, created="x")
        eng.setup()
        b_deck = len(eng.state.players["B"].deck)
        eng._run_effects((rider,), fx.OnHit, "A")  # A owns the rider -> flips B's deck
        assert len(eng.state.players["B"].deck) == b_deck - delta  # flipped iff taken


def test_remove_from_play_sends_a_chosen_opponent_board_card_to_discard() -> None:
    # #46: board disruption — the ACTOR discards a card the OPPONENT has in play,
    # aimed by the selector; non-matching board cards are untouched.
    eng = _fresh()
    b = eng.state.players["B"]
    lead = replace(_attack(AtkType.STRIKE, PlayOrder.LEAD), db_uuid="lead")
    fu = replace(_attack(AtkType.GRAPPLE, PlayOrder.FOLLOWUP), db_uuid="fu")
    b.in_play = [lead, fu]
    eng._act_remove_from_play(
        fx.RemoveFromPlay(selector=fx.CardFilter(play_order=PlayOrder.FOLLOWUP), who=fx.Who.OPP),
        "A",  # A is the actor; OPP = B
    )
    assert fu in b.discard and fu not in b.in_play  # the aimed card was discarded
    assert lead in b.in_play  # the non-matching Lead stayed on the board
    ev = json.loads(eng.state.log.to_lines()[-1])
    assert ev["type"] == "discard" and ev["from"] == "in_play" and ev["player"] == "B"


def test_remove_from_play_on_empty_board_is_a_noop() -> None:
    eng = _fresh()
    eng.state.players["B"].in_play = []
    eng._act_remove_from_play(fx.RemoveFromPlay(who=fx.Who.OPP), "A")  # nothing to remove
    assert eng.state.players["B"].discard == []


def test_movement_hidden_flag_tracks_private_endpoints() -> None:
    # §8 information model: draws (deck->hand) are hidden; discards (->public
    # pile) never are. A real game exercises both.
    events = [json.loads(x) for x in _play(6).state.log.to_lines()[1:]]
    draws = [e for e in events if e["type"] == "draw"]
    discards = [e for e in events if e["type"] == "discard"]
    assert draws and all(e["hidden"] for e in draws)  # every draw is hidden
    assert all(not e["hidden"] for e in discards)  # discards land in a public pile


def test_unsupported_action_is_logged_never_dropped() -> None:
    weird = fx.Effect(
        trigger=fx.OnWinTurn(),
        actions=(fx.Unsupported(raw_text="do something odd", reason="no grammar"),),
        raw_clause="odd",
        source=fx.EffectSource.GIMMICK,
    )
    eng = Engine(
        make_deck("A", with_effects(bull(), (weird,))),
        make_deck("B", fae()),
        HeuristicPolicy(),
        HeuristicPolicy(),
        seed=3,
        created="x",
    )
    eng.play()
    types = collections.Counter(json.loads(x)["type"] for x in eng.state.log.to_lines()[1:])
    assert types["unsupported"] > 0


def test_start_of_match_crowd_effect_fires_at_setup() -> None:
    entrance_eff = fx.Effect(
        trigger=fx.StartOfMatch(),
        actions=(fx.CrowdMeter(delta=1),),
        raw_clause="start at CM1",
        source=fx.EffectSource.ENTRANCE,
    )
    da, db = bull_vs_fae()
    da = replace(da, entrance=replace(da.entrance, effects=(entrance_eff,)))
    eng = Engine(da, db, HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    assert eng.state.crowd_meter == 1


# -- LoseBy win conditions ---------------------------------------------------


# -- first-turn redraw (#44: per-player, first won turn, ordered bury, up to N) ---


def _leadless_hand(eng: Engine, key: str, numbers: tuple[int, ...]) -> list[Card]:
    player = eng.state.players[key]
    hand = [next(c for c in player.deck if c.number == n) for n in numbers]
    for c in hand:
        player.deck.remove(c)
    player.hand = list(hand)
    return hand


def test_setup_no_longer_runs_the_first_turn_redraw() -> None:
    # #44: the redraw is NOT a setup step — nobody is flagged and nothing is buried.
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    assert not any(p.flags.get("had_first_turn") for p in eng.state.players.values())
    assert all(json.loads(line)["type"] != "bury" for line in eng.state.log.to_lines()[1:])


def test_first_turn_option_fires_at_most_once_per_player() -> None:
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    a = eng.state.players["A"]
    _leadless_hand(eng, "A", (13, 14, 15))  # no Lead -> heuristic redraws
    eng._first_turn_option("A")
    assert a.flags["had_first_turn"]
    hand_after = list(a.hand)
    eng._first_turn_option("A")  # spent — a no-op now
    assert a.hand == hand_after


def test_first_turn_redraw_orders_the_bury_and_draws_up_to_n() -> None:
    # #44: player buries the revealed hand in a CHOSEN order (not random) and draws
    # UP TO that many (here 2 of 3). The reveal makes the moved cards public.
    class Mull(Policy):
        def __init__(self) -> None:
            super().__init__("mull")

        def choose(self, point, legal, state, key):  # type: ignore[no-untyped-def]
            if point == "mulligan":
                return next(o for o in legal if o["kind"] == "redraw")
            if point == "mulligan_bury":
                return min(legal, key=lambda o: o["number"])  # bury in ascending order
            if point == "mulligan_draw":
                return next(o for o in legal if o["n"] == 2)  # draw only 2 of the 3
            return legal[0]

    eng = Engine(*bull_vs_fae(), Mull(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    a = eng.state.players["A"]
    _leadless_hand(eng, "A", (15, 13, 14))
    eng._first_turn_option("A")
    assert [c.number for c in a.deck[-3:]] == [13, 14, 15]  # buried ascending, bottom
    assert len(a.hand) == 2  # drew up to 2, not all 3
    events = [json.loads(line) for line in eng.state.log.to_lines()[1:]]
    bury = next(e for e in events if e["type"] == "bury")
    assert bury["from"] == "hand" and bury["hidden"] is False  # revealed -> public


def test_first_turn_redraw_not_offered_with_a_lead_in_hand() -> None:
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    a = eng.state.players["A"]
    hand = _leadless_hand(eng, "A", (13, 14))
    lead = next(c for c in a.deck if c.number == 1)  # a Lead
    a.deck.remove(lead)
    a.hand = [*hand, lead]
    before = list(a.hand)
    eng._first_turn_option("A")  # has a Lead -> option not offered, hand kept
    assert a.hand == before and a.flags["had_first_turn"]


def test_lose_by_disqualification_when_a_card_is_stopped() -> None:
    dq = fx.Effect(
        trigger=fx.OnStop(dir=fx.Direction.YOURS),
        actions=(fx.LoseBy(kind=fx.LoseKind.DISQUALIFICATION, who=fx.Who.SELF),),
        raw_clause="if stopped, DQ",
        source=fx.EffectSource.CARD,
    )
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    eng.state.turn_no = 1
    a_deck = eng.state.players["A"].deck
    attack = replace(a_deck[0], atk_type=AtkType.STRIKE, play_order=PlayOrder.LEAD, effects=(dq,))
    stopper = replace(eng.state.players["B"].deck[0], atk_type=AtkType.SUBMISSION)
    eng._apply_stop("A", "B", attack, stopper)
    assert eng.result == GameResult("B", "disqualification", 1)


def test_stop_enters_the_defenders_board_not_discard() -> None:
    # DESIGN.md §6 / walkthrough: the stopping card is PLAYED onto the defender's
    # board and persists; only the stopped attack goes to the attacker's discard.
    eng = _fresh()
    attack = _attack(AtkType.GRAPPLE, PlayOrder.LEAD)
    stopper = replace(eng.state.players["B"].deck[0], atk_type=AtkType.STRIKE, db_uuid="stp")
    eng._apply_stop("A", "B", attack, stopper)
    assert stopper in eng.state.players["B"].in_play
    assert stopper not in eng.state.players["B"].discard
    assert attack in eng.state.players["A"].discard


def test_followup_stop_enters_play_with_no_lead() -> None:
    # A Follow Up used as a stop enters play even with no Lead — stopping bypasses
    # the play-sequence gate — so it can then enable a Finish (DESIGN.md §6, todo #33).
    from srg_sim.engine import _playable

    eng = _fresh()
    attack = _attack(AtkType.STRIKE, PlayOrder.LEAD)
    stopper = _attack(AtkType.SUBMISSION, PlayOrder.FOLLOWUP)
    assert not eng.state.players["B"].in_play
    eng._apply_stop("A", "B", attack, stopper)
    board = eng.state.players["B"].in_play
    assert stopper in board  # FU sits on the board with no Lead beneath it
    fin = _attack(AtkType.SUBMISSION, PlayOrder.FINISH)
    assert _playable(board, fin)  # and now enables a Finish


# -- stops (text-driven: a card stops only via its parsed Stop effects) -------


def _fresh() -> Engine:
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.state.turn_no = 1  # decks are full (no setup), cards found by number in deck order
    return eng


def _attack(atk: AtkType, order: PlayOrder) -> Card:
    return Card(db_uuid="atk", name="Atk", number=2, atk_type=atk, play_order=order)


def _hand_card(eng: Engine, key: str, number: int) -> Card:
    card = next(c for c in eng.state.players[key].deck if c.number == number)
    eng.state.players[key].hand = [card]
    return card


def test_stop_matches_order_and_type() -> None:
    # Demo card 1 (Strike) stops Grapple *Leads* only.
    eng = _fresh()
    card1 = _hand_card(eng, "B", 1)
    assert card1 in eng._legal_stops("B", "A", _attack(AtkType.GRAPPLE, PlayOrder.LEAD))
    assert card1 not in eng._legal_stops("B", "A", _attack(AtkType.GRAPPLE, PlayOrder.FINISH))
    assert card1 not in eng._legal_stops("B", "A", _attack(AtkType.STRIKE, PlayOrder.LEAD))


def test_card_without_stop_effect_cannot_stop() -> None:
    # Demo card 7 is an incremental-value Lead with no Stop effect.
    eng = _fresh()
    _hand_card(eng, "B", 7)
    assert eng._legal_stops("B", "A", _attack(AtkType.GRAPPLE, PlayOrder.LEAD)) == []


def test_stop_any_covers_every_ordering_of_its_type() -> None:
    # Demo card 25 (Strike) is a stop-any: stops Grapple of any ordering.
    eng = _fresh()
    card25 = _hand_card(eng, "B", 25)
    for order in (PlayOrder.LEAD, PlayOrder.FOLLOWUP, PlayOrder.FINISH):
        assert card25 in eng._legal_stops("B", "A", _attack(AtkType.GRAPPLE, order))
    assert card25 not in eng._legal_stops("B", "A", _attack(AtkType.STRIKE, PlayOrder.FINISH))


def test_skill_stop_gated_by_condition() -> None:
    # Demo card 15 (Submission skill stop) stops Strike iff defender Submission > attacker's.
    eng = _fresh()
    card15 = _hand_card(eng, "B", 15)  # B=Fae Submission 9 vs A=Bull Submission 8 -> online
    strike = _attack(AtkType.STRIKE, PlayOrder.FINISH)
    assert card15 in eng._legal_stops("B", "A", strike)
    # A card in play that lowers Fae's Submission below Bull's flips the stop offline.
    debuff = fx.Effect(
        trigger=fx.Static(),
        actions=(fx.BuffSkill(Skill.SUBMISSION, -3, fx.Who.SELF, fx.Duration.WHILE_IN_PLAY),),
        duration=fx.Duration.WHILE_IN_PLAY,
    )
    eng.state.players["B"].in_play.append(
        Card(
            db_uuid="d",
            name="D",
            number=1,
            atk_type=AtkType.STRIKE,
            play_order=PlayOrder.LEAD,
            effects=(debuff,),
        )
    )
    assert card15 not in eng._legal_stops("B", "A", strike)  # Fae Sub 9-3=6 < Bull 8


def test_see1_stop_needs_opp_type_in_play() -> None:
    # Demo card 19 (Strike see-1) stops Grapple only if the opponent already has a Grapple in play.
    eng = _fresh()
    card19 = _hand_card(eng, "B", 19)
    grapple = _attack(AtkType.GRAPPLE, PlayOrder.FINISH)
    assert card19 not in eng._legal_stops("B", "A", grapple)
    eng.state.players["A"].in_play.append(_attack(AtkType.GRAPPLE, PlayOrder.LEAD))
    assert card19 in eng._legal_stops("B", "A", grapple)


def test_bump_makes_both_players_draw(monkeypatch: pytest.MonkeyPatch) -> None:
    # On a tied turn roll both players bump: draw a card, then re-roll (mechanics §2).
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng.setup()
    eng.state.turn_no = 1
    a0, b0 = len(eng.state.players["A"].deck), len(eng.state.players["B"].deck)
    # _roll_for now returns (skill, value); tie once (5,5) -> bump -> then A wins (6,5).
    rolls = iter([(Skill.POWER, 5), (Skill.POWER, 5), (Skill.POWER, 6), (Skill.POWER, 5)])
    monkeypatch.setattr(eng, "_roll_for", lambda key, use_pending: next(rolls))
    assert eng._roll_off() == "A"
    assert len(eng.state.players["A"].deck) == a0 - 1  # each drew exactly once on the bump
    assert len(eng.state.players["B"].deck) == b0 - 1


def test_stopped_card_fires_none_of_its_text() -> None:
    # srg-rules-confirmed / #45: the stop window precedes a card's OnPlay text, so a
    # stopped card's effect (here a Draw) must NOT resolve — only the attack is discarded.
    class StopFirst(Policy):
        def __init__(self) -> None:
            super().__init__("stop-first")

        def choose(self, point, legal, state, key):  # type: ignore[no-untyped-def]
            return next((o for o in legal if o["kind"] == "stop"), legal[0])

    draw2 = fx.Effect(
        trigger=fx.OnPlay(),
        actions=(fx.Draw(n=2),),
        raw_clause="on play draw 2",
        source=fx.EffectSource.CARD,
    )
    attack = replace(
        _attack(AtkType.GRAPPLE, PlayOrder.LEAD), effects=(draw2,), db_uuid="atk", number=2
    )

    # Stopped: demo card 25 (Strike stop-any) answers the Grapple Lead -> no draw.
    eng = Engine(*bull_vs_fae(), HeuristicPolicy(), StopFirst(), seed=1, created="x")
    eng.setup()
    eng.state.turn_no = 1
    a, b = eng.state.players["A"], eng.state.players["B"]
    b.hand = [next(c for c in b.deck if c.number == 25)]
    deck_before = len(a.deck)
    assert eng._resolve_play("A", "B", attack) is False
    assert len(a.deck) == deck_before  # OnPlay Draw(2) was cancelled by the stop
    assert attack in a.discard

    # Unstopped (empty defender hand): the same card's OnPlay Draw(2) DOES fire.
    eng2 = Engine(*bull_vs_fae(), HeuristicPolicy(), HeuristicPolicy(), seed=1, created="x")
    eng2.setup()
    eng2.state.turn_no = 1
    a2 = eng2.state.players["A"]
    eng2.state.players["B"].hand = []
    hand_before = len(a2.hand)
    assert eng2._resolve_play("A", "B", replace(attack, db_uuid="atk2")) is True
    assert len(a2.hand) == hand_before + 2  # drew 2, card resolved into play


def test_heuristic_actually_plays_stops() -> None:
    # Regression: stop options must be tagged so the heuristic defender uses them
    # (the persistent board exposed a kind-mismatch that made it never stop).
    total = 0
    for seed in range(20):
        eng = _play(seed, HeuristicPolicy(), HeuristicPolicy())
        total += sum(1 for x in eng.state.log.to_lines()[1:] if json.loads(x)["type"] == "stop")
    assert total > 0


# -- persistent board + cross-turn chain (DESIGN.md §6) ----------------------


def test_playable_is_order_only_against_the_board() -> None:
    from srg_sim.engine import _playable

    lead = _attack(AtkType.STRIKE, PlayOrder.LEAD)
    fu = _attack(AtkType.STRIKE, PlayOrder.FOLLOWUP)
    fin = _attack(AtkType.STRIKE, PlayOrder.FINISH)
    assert _playable([], lead)  # a Lead is always playable
    assert _playable([lead], lead)  # you may stack another Lead
    assert not _playable([], fu)  # a Follow Up needs a Lead in play
    assert _playable([lead], fu)
    assert not _playable([lead], fin)  # a Finish needs a Follow Up, not just a Lead
    assert _playable([lead, fu], fin)


def test_resolved_card_persists_in_play_across_the_turn() -> None:
    eng = _fresh()
    eng.state.players["B"].hand = []  # defender cannot stop
    lead = next(c for c in eng.state.players["A"].deck if c.number == 7)  # plain Lead, no stop
    eng.state.players["A"].hand = [lead]
    eng._take_turn_action("A")
    assert lead in eng.state.players["A"].in_play  # board is NOT cleared each turn


def test_breakout_clears_both_boards_and_bumps_crowd_meter() -> None:
    eng = _fresh()
    eng.state.players["A"].in_play = [_attack(AtkType.STRIKE, PlayOrder.LEAD)]
    eng.state.players["B"].in_play = [_attack(AtkType.GRAPPLE, PlayOrder.LEAD)]
    eng._on_broken_out("A")
    assert eng.state.players["A"].in_play == []
    assert eng.state.players["B"].in_play == []
    assert eng.state.crowd_meter == 1


# -- finish bonuses: whole-combo sum + flat finish-roll bonus (§5) -----------


def _finish_value(eng: Engine) -> int:
    for line in eng.state.log.to_lines():
        e = json.loads(line)
        if e.get("type") == "finish_attempt":
            return int(e["value"])
    raise AssertionError("no finish_attempt logged")


def _combo_card(number: int, order: PlayOrder, skill: Skill, delta: int) -> Card:
    return Card(
        db_uuid=f"c{number}",
        name=f"C{number}",
        number=number,
        atk_type=AtkType.STRIKE,
        play_order=order,
        finish_bonuses=((skill, delta),),
    )


def test_finish_sums_the_whole_in_play_combo_not_just_the_finish_card(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    eng = _fresh()
    monkeypatch.setattr(eng.state.rng, "roll", lambda: Skill.STRIKE)
    monkeypatch.setattr(eng, "_stat", lambda key, skill: 0)  # isolate the bonus
    lead = _combo_card(7, PlayOrder.LEAD, Skill.STRIKE, 1)
    fu = _combo_card(19, PlayOrder.FOLLOWUP, Skill.STRIKE, 2)
    fin = _combo_card(28, PlayOrder.FINISH, Skill.STRIKE, 3)
    eng.state.players["A"].in_play = [lead, fu, fin]
    eng._finish_sequence("A", "B", fin)
    assert _finish_value(eng) == 6  # 1 + 2 + 3, the full Lead+FU+Finish combo


def test_flat_finish_roll_bonus_adds_any_skill(monkeypatch: pytest.MonkeyPatch) -> None:
    eng = _fresh()
    monkeypatch.setattr(eng.state.rng, "roll", lambda: Skill.POWER)  # no combo bonus for Power
    monkeypatch.setattr(eng, "_stat", lambda key, skill: 0)
    boost = fx.Effect(trigger=fx.Static(), actions=(fx.FinishRollBonus(4),))
    fin = Card(
        db_uuid="f",
        name="F",
        number=28,
        atk_type=AtkType.STRIKE,
        play_order=PlayOrder.FINISH,
        effects=(boost,),
    )
    eng.state.players["A"].in_play = [fin]
    eng._finish_sequence("A", "B", fin)
    assert _finish_value(eng) == 4  # flat +4 regardless of the rolled skill


# -- discard: hand-cap + forced discards route through the owner (§6/§7) ------


def test_hand_cap_discards_down_to_ten_by_owner_choice() -> None:
    eng = _fresh()
    hand = [next(c for c in eng.state.players["A"].deck if c.number == n) for n in range(1, 13)]
    eng.state.players["A"].hand = list(hand)  # 12 > cap of 10
    eng._hand_cap("A")
    assert len(eng.state.players["A"].hand) == 10
    assert len(eng.state.players["A"].discard) == 2  # exactly the excess shed


def test_draw_over_cap_discards_immediately_not_at_end_of_turn() -> None:
    # DESIGN.md §6 / todo #28: any draw that puts a player over max caps right then,
    # inside _draw — a top-deck to 11 forces a discard-down before the play action.
    eng = _fresh()
    hand = [next(c for c in eng.state.players["A"].deck if c.number == n) for n in range(1, 11)]
    eng.state.players["A"].hand = list(hand)  # exactly at the cap of 10
    eng._draw("A", 1)  # 11th card must be shed immediately
    assert len(eng.state.players["A"].hand) == 10
    assert len(eng.state.players["A"].discard) == 1


def _static_hand_mod(delta: int, who: fx.Who) -> fx.Effect:
    return fx.Effect(
        trigger=fx.Static(),
        actions=(fx.MaxHandSize(delta, who),),
        duration=fx.Duration.WHILE_IN_PLAY,
    )


def test_hand_cap_respects_a_raised_self_maximum() -> None:
    # DESIGN.md §6 / todo #37: a Static MaxHandSize on your own board raises the cap,
    # so a hand that would overflow the base 10 is kept intact.
    eng = _fresh()
    eng.state.players["A"].competitor = replace(
        eng.state.players["A"].competitor, effects=(_static_hand_mod(2, fx.Who.SELF),)
    )
    hand = [next(c for c in eng.state.players["A"].deck if c.number == n) for n in range(1, 13)]
    eng.state.players["A"].hand = list(hand)  # 12 cards, cap now 12
    eng._hand_cap("A")
    assert len(eng.state.players["A"].hand) == 12  # nothing shed
    assert eng.state.players["A"].discard == []


def test_opponent_card_lowering_max_forces_immediate_discard() -> None:
    # DESIGN.md §6 / todo #37: the cap is continuous — a card entering A's play that
    # lowers B's max hand size makes B discard down right then, with no draw of B's.
    eng = _fresh()
    b_hand = [next(c for c in eng.state.players["B"].deck if c.number == n) for n in range(1, 11)]
    eng.state.players["B"].hand = list(b_hand)  # B sits at the base cap of 10
    card = replace(
        next(c for c in eng.state.players["A"].deck if c.number == 7),
        effects=(_static_hand_mod(-2, fx.Who.OPP),),
    )
    eng.state.players["A"].in_play.append(card)  # B's max is now 8
    eng._enforce_hand_caps()
    assert len(eng.state.players["B"].hand) == 8  # B shed the excess with no draw
    assert len(eng.state.players["B"].discard) == 2
    assert eng.state.players["A"].hand == []  # A's own hand untouched


def test_opponent_forced_discard_targets_and_lets_the_owner_choose() -> None:
    eng = _fresh()
    a_hand = [next(c for c in eng.state.players["A"].deck if c.number == 7)]
    b_hand = [next(c for c in eng.state.players["B"].deck if c.number == n) for n in (7, 28)]
    eng.state.players["A"].hand = list(a_hand)
    eng.state.players["B"].hand = list(b_hand)
    # A plays a card reading "your opponent discards 1": B (the owner) chooses.
    eng._act_discard(fx.Discard(count=1, who=fx.Who.OPP), "A")
    assert len(eng.state.players["B"].hand) == 1  # B shed one
    assert eng.state.players["A"].hand == a_hand  # A's own hand untouched
    # B's heuristic protects the Finish (28) and sheds the dead Lead (7).
    assert eng.state.players["B"].hand[0].number == 28


def test_random_discard_uses_the_seeded_rng_not_the_policy() -> None:
    eng = _fresh()
    hand = [next(c for c in eng.state.players["A"].deck if c.number == n) for n in (7, 28)]
    eng.state.players["A"].hand = list(hand)
    eng._discard_from_hand("A", 1, random=True)
    assert len(eng.state.players["A"].hand) == 1
    assert len(eng.state.players["A"].discard) == 1  # a card left the hand for the pile


# -- snapshot mid-game -------------------------------------------------------


def test_mid_game_state_snapshot_round_trips() -> None:
    from srg_sim.state import GameState

    eng = _play(9)
    snap = eng.state.to_dict()
    assert GameState.from_dict(snap).to_dict() == snap
