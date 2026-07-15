"""Tests for interactive human play (DESIGN.md §7, todo #42).

A scripted ``ask`` drives :class:`~srg_sim.interactive.HumanPolicy` with no real
terminal, so a full human-vs-engine match runs headless. Rendering is asserted to
be built solely from the observable view — the opponent's hand is never shown.
"""

from __future__ import annotations

from srg_sim.cards import EntranceCard
from srg_sim.engine import Engine
from srg_sim.interactive import HumanPolicy, render_options, render_view
from srg_sim.policy import SmartPasser
from srg_sim.rng import SeededRNG
from srg_sim.state import GameState, PlayerState

from tests.demo_decks import bull, bull_vs_fae, fae


def _state() -> GameState:
    ent = EntranceCard("e", "E")
    a = PlayerState(competitor=bull(), entrance=ent)
    b = PlayerState(competitor=fae(), entrance=ent)
    # Give B a hand so the view has something to (not) leak.
    b.deck = list(make_hand())
    b.draw(3)
    return GameState(players={"A": a, "B": b}, rng=SeededRNG(0))


def make_hand() -> list:
    return list(bull_vs_fae()[1].cards)


# -- rendering is redacted ---------------------------------------------------


def test_render_view_shows_own_hand_and_opponent_count_only() -> None:
    state = _state()
    lines = render_view(state, "A")
    blob = "\n".join(lines)
    assert "you are A" in blob
    # Opponent B holds 3 cards: the view names the count, never the card identities.
    assert "hand: 3 cards" in blob
    for card in state.players["B"].hand:
        assert card.name not in blob  # no opponent card identity leaks


def test_render_options_is_a_numbered_menu() -> None:
    legal = [
        {"kind": "play", "number": 5, "card": "A-05", "order": "Lead", "atk_type": "Grapple"},
        {"kind": "pass"},
    ]
    lines = render_options("turn_action", legal)
    assert lines[0] == "decision: turn_action"
    assert lines[1].startswith("  1) play #5")
    assert lines[2].strip() == "2) pass"


def test_render_options_labels_the_none_stop_with_what_it_defends() -> None:
    legal = [{"kind": "none", "vs_order": "Finish", "vs_type": "Strike"}]
    line = render_options("stop", legal)[1]
    assert "do not stop" in line and "Finish" in line and "Strike" in line


# -- policy behaviour --------------------------------------------------------


def test_human_policy_picks_the_selected_option() -> None:
    shown: list[str] = []
    picks = iter(["2"])
    policy = HumanPolicy(out=shown.append, ask=lambda _: next(picks))
    legal = [{"kind": "pass"}, {"kind": "play", "number": 5, "card": "A-05"}]
    chosen = policy.choose("turn_action", legal, _state(), "A")
    assert chosen == legal[1]
    assert any("decision: turn_action" in line for line in shown)


def test_human_policy_reprompts_on_bad_input() -> None:
    shown: list[str] = []
    picks = iter(["0", "nine", "1"])
    policy = HumanPolicy(out=shown.append, ask=lambda _: next(picks))
    legal = [{"kind": "pass"}, {"kind": "play", "number": 5, "card": "A-05"}]
    chosen = policy.choose("turn_action", legal, _state(), "A")
    assert chosen == legal[0]
    assert sum("please enter a number" in line for line in shown) == 2


def test_full_headless_human_vs_engine_game_completes() -> None:
    da, db = bull_vs_fae()
    human = HumanPolicy(out=lambda _: None, ask=lambda _: "1")  # always the first option
    eng = Engine(da, db, human, SmartPasser(), seed=3, created="2026-07-15", kind="real")
    result = eng.play()
    assert result.winner in {"A", "B", "draw"}
    assert eng.state.log is not None
    assert eng.state.log.header.kind == "real"
    assert eng.state.log.header.players["A"].policy == "human"
