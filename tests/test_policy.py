"""Tests for the HeuristicPolicy's playstyle rules (DESIGN.md §7, sim-fix)."""

from __future__ import annotations

from srg_sim.cards import EntranceCard
from srg_sim.policy import (
    AggressiveBuilder,
    HeuristicPolicy,
    Newbie,
    SmartPasser,
    has_stop_effect,
)
from srg_sim.rng import SeededRNG
from srg_sim.state import GameState, PlayerState

from tests.demo_decks import bull, fae, make_deck

POLICY = HeuristicPolicy()


def _state() -> GameState:
    ent = EntranceCard("e", "E")
    a = PlayerState(competitor=bull(), entrance=ent)
    b = PlayerState(competitor=fae(), entrance=ent)
    return GameState(players={"A": a, "B": b}, rng=SeededRNG(1))


def _card(number: int):  # type: ignore[no-untyped-def]
    return next(c for c in make_deck("A", bull()).cards if c.number == number)


def _play_opt(card) -> dict:  # type: ignore[no-untyped-def]
    return {
        "kind": "play",
        "number": card.number,
        "card": card.db_uuid,
        "order": card.play_order.value,
    }


# --- defense: reserve stops for Finishes -----------------------------------


def test_stops_a_finish() -> None:
    legal = [
        {"kind": "none", "vs_order": "Finish", "vs_type": "Strike"},
        {"kind": "stop", "number": 15},
    ]
    assert POLICY._at_stop(legal, _state(), "A")["kind"] == "stop"


def test_lets_a_lead_resolve_to_save_the_stop() -> None:
    legal = [
        {"kind": "none", "vs_order": "Lead", "vs_type": "Grapple"},
        {"kind": "stop", "number": 1},
    ]
    assert POLICY._at_stop(legal, _state(), "A")["kind"] == "none"


# --- offense: build minimally, hold online stops ---------------------------


def test_goes_for_the_finish_when_playable() -> None:
    fin, lead = _card(28), _card(7)
    legal = [_play_opt(lead), _play_opt(fin), {"kind": "pass"}]
    assert POLICY._at_turn_action(legal, _state(), "A")["number"] == 28


def test_builds_with_the_least_valuable_card_holding_online_stops() -> None:
    state = _state()
    plain_lead = _card(7)  # incremental Lead, no stop
    stop_lead = _card(1)  # Lead that stops Grapple Leads (online: unconditional)
    state.players["A"].hand = [stop_lead, plain_lead]
    legal = [_play_opt(stop_lead), _play_opt(plain_lead), {"kind": "pass"}]
    # Board empty -> needs a Lead; plays the non-stop and keeps the stop in hand.
    assert POLICY._at_turn_action(legal, state, "A")["number"] == 7


def test_passes_when_the_chain_is_already_built_without_a_finish() -> None:
    state = _state()
    state.players["A"].in_play = [_card(7), _card(19)]  # a Lead + a Follow Up already down
    plain_lead = _card(10)
    state.players["A"].hand = [plain_lead]
    legal = [_play_opt(plain_lead), {"kind": "pass"}]
    # Lead + Follow Up already in play and no Finish playable -> hold and pass.
    assert POLICY._at_turn_action(legal, state, "A")["kind"] == "pass"


def test_passes_rather_than_spend_an_online_stop_to_build() -> None:
    state = _state()
    stop_lead = _card(1)  # the only Lead is an online stop
    state.players["A"].hand = [stop_lead]
    legal = [_play_opt(stop_lead), {"kind": "pass"}]
    assert POLICY._at_turn_action(legal, state, "A")["kind"] == "pass"  # hoard it


# --- pass/bury: recycle the most valuable card ------------------------------


def test_bury_recycles_a_finish_before_a_stop_or_dead_card() -> None:
    state = _state()
    dead, stop, finish = _card(7), _card(1), _card(28)
    state.players["A"].discard = [dead, stop, finish]
    legal = [_play_opt(dead), _play_opt(stop), _play_opt(finish)]
    assert POLICY._at_bury(legal, state, "A")["number"] == 28


def test_bury_prefers_a_stop_over_a_dead_card() -> None:
    state = _state()
    dead, stop = _card(7), _card(1)
    state.players["A"].discard = [dead, stop]
    legal = [_play_opt(dead), _play_opt(stop)]
    assert POLICY._at_bury(legal, state, "A")["number"] == 1


# --- discard: shed the least valuable, protect the line --------------------


def _disc_opt(card) -> dict:  # type: ignore[no-untyped-def]
    return {"kind": "discard", "number": card.number, "card": card.db_uuid}


def test_discard_sheds_a_dead_card_before_a_stop_or_finish() -> None:
    state = _state()
    state.players["A"].in_play = [_card(7)]  # a Lead down -> the chain needs a Follow Up
    dead, online_stop, finish = _card(8), _card(1), _card(28)
    state.players["A"].hand = [dead, online_stop, finish]
    legal = [_disc_opt(dead), _disc_opt(online_stop), _disc_opt(finish)]
    assert POLICY._at_discard(legal, state, "A")["number"] == 8  # the dead card


def test_discard_protects_a_finish_and_needed_piece_shedding_an_offline_stop() -> None:
    state = _state()  # empty board -> the chain needs a Lead
    needed_lead, offline_stop, finish = _card(7), _card(19), _card(28)
    state.players["A"].hand = [needed_lead, offline_stop, finish]
    legal = [_disc_opt(needed_lead), _disc_opt(offline_stop), _disc_opt(finish)]
    assert POLICY._at_discard(legal, state, "A")["number"] == 19  # the offline see-1 stop


def test_discard_prefers_an_offline_stop_over_an_online_one() -> None:
    state = _state()
    state.players["A"].in_play = [_card(7), _card(13)]  # Lead+Follow Up down: nothing is "needed"
    online_stop, offline_stop = _card(1), _card(19)  # a Lead stop (online) vs a see-1 FU (offline)
    state.players["A"].hand = [online_stop, offline_stop]
    legal = [_disc_opt(online_stop), _disc_opt(offline_stop)]
    assert POLICY._at_discard(legal, state, "A")["number"] == 19  # keep the ready defense


# --- helper ----------------------------------------------------------------


def test_has_stop_effect() -> None:
    assert has_stop_effect(_card(1))  # 1-3 stop Leads
    assert has_stop_effect(_card(25))  # 25-27 stop-any
    assert not has_stop_effect(_card(7))  # 7-12 incremental value
    assert not has_stop_effect(_card(28))  # Finishes don't stop


def test_mulligan_keeps_a_hand_with_a_lead() -> None:
    state = _state()
    state.players["A"].hand = [_card(7)]  # a Lead
    legal = [{"kind": "redraw"}, {"kind": "keep"}]
    assert POLICY._at_mulligan(legal, state, "A")["kind"] == "keep"
    state.players["A"].hand = [_card(19)]  # a Follow Up, no Lead
    assert POLICY._at_mulligan(legal, state, "A")["kind"] == "redraw"


# --- player profiles (todo #32): distinct skill levels ----------------------

AGGRO = AggressiveBuilder()
SMART = SmartPasser()
NEWBIE = Newbie()


def test_aggressive_builder_opens_a_lead_without_a_finish() -> None:
    # The validated aggressive default == HeuristicPolicy: build onto an empty board.
    state = _state()
    plain_lead = _card(7)
    state.players["A"].hand = [plain_lead]
    legal = [_play_opt(plain_lead), {"kind": "pass"}]
    assert AGGRO._at_turn_action(legal, state, "A")["number"] == 7


def test_smart_passer_hoards_when_it_holds_no_finish() -> None:
    # Empty board, a playable non-stop Lead, but NO Finish in hand -> pass to hoard.
    state = _state()
    plain_lead = _card(7)
    state.players["A"].hand = [plain_lead]
    legal = [_play_opt(plain_lead), {"kind": "pass"}]
    assert SMART._at_turn_action(legal, state, "A")["kind"] == "pass"


def test_smart_passer_builds_when_it_holds_a_finish() -> None:
    # Holding the Finish, the smart player builds toward the combo (plays the Lead).
    state = _state()
    plain_lead, finish = _card(7), _card(28)
    state.players["A"].hand = [plain_lead, finish]  # Finish not yet playable (no FU in play)
    legal = [_play_opt(plain_lead), {"kind": "pass"}]
    assert SMART._at_turn_action(legal, state, "A")["number"] == 7


def test_smart_passer_still_throws_a_playable_finish() -> None:
    state = _state()
    fin, lead = _card(28), _card(7)
    legal = [_play_opt(lead), _play_opt(fin), {"kind": "pass"}]
    assert SMART._at_turn_action(legal, state, "A")["number"] == 28


def test_newbie_greedily_opens_a_lead_like_the_aggressive_player() -> None:
    state = _state()
    plain_lead = _card(7)
    state.players["A"].hand = [plain_lead]
    legal = [_play_opt(plain_lead), {"kind": "pass"}]
    assert NEWBIE._at_turn_action(legal, state, "A")["number"] == 7


def test_newbie_will_not_play_a_stop_offensively() -> None:
    # The only playable Lead is a stop -> the newbie won't burn it as an attack; passes.
    state = _state()
    stop_lead = _card(1)
    state.players["A"].hand = [stop_lead]
    legal = [_play_opt(stop_lead), {"kind": "pass"}]
    assert NEWBIE._at_turn_action(legal, state, "A")["kind"] == "pass"


def test_newbie_stops_eagerly_wasting_it_on_a_lead() -> None:
    # Where the heuristic saves a stop vs a Lead, the newbie spends it immediately.
    legal = [
        {"kind": "none", "vs_order": "Lead", "vs_type": "Grapple"},
        {"kind": "stop", "number": 1},
    ]
    assert NEWBIE._at_stop(legal, _state(), "A")["kind"] == "stop"


def test_newbie_discards_carelessly_leftmost_even_a_finish() -> None:
    # No protection of the line: the newbie sheds whatever is leftmost (here a Finish).
    state = _state()
    finish, dead = _card(28), _card(8)
    state.players["A"].hand = [finish, dead]
    legal = [_disc_opt(finish), _disc_opt(dead)]
    assert NEWBIE._at_discard(legal, state, "A")["number"] == 28
