"""Engine validation suite (DESIGN.md §11).

The finish/stop *math* parity lives in ``test_finish.py`` / ``test_stops.py``;
this module validates the assembled engine: a gimmick-free duel is ≈50/50, the
seeded turn roll matches its closed-form expectation, and a game is byte-for-byte
deterministic and replayable under its seed.
"""

from __future__ import annotations

import collections
import importlib.util
import json
from pathlib import Path
from typing import Any

import pytest
from srg_sim.cards import AtkType, Card, PlayOrder, Skill
from srg_sim.engine import Engine
from srg_sim.gamelog import matches
from srg_sim.policy import HeuristicPolicy, RandomPolicy

from tests.demo_decks import bull, bull_gimmick, fae, fae_gimmick, make_deck, vanilla

GAMES = 400
ROLLS = 1000

# Turn-roll parity: reproduce tournament_turnsim's self-check win rates (§11). The
# reference numbers are the golden targets, baked so CI needs no fae_comp checkout.
TURN_ROLL_STREAM = 20_000  # rolls per parity measurement (deterministic under the seed)
REF_BULL_VS_VANILLA = 0.541
REF_BULL_VS_FAE = 0.459
PARITY_TOL = 0.015  # within ~1.5 points of the reference's Monte-Carlo figure

# A filler card to keep a long roll-off stream from depleting decks on bumps.
_FILLER = Card(
    db_uuid="fill", name="Filler", number=7, atk_type=AtkType.STRIKE, play_order=PlayOrder.FOLLOWUP
)


def test_text_driven_stops_engage_under_skilled_play() -> None:
    """Persistent board + text-driven stops -> defenders actually spend stops
    contesting attacks (regression against a null-defense sim; DESIGN.md §11)."""
    total_stops = 0
    for seed in range(30):
        eng = Engine(
            make_deck("A", bull()),
            make_deck("B", fae()),
            HeuristicPolicy(),
            HeuristicPolicy(),
            seed=seed,
            created="x",
        )
        eng.play()
        total_stops += sum(
            1 for x in eng.state.log.to_lines()[1:] if json.loads(x)["type"] == "stop"
        )
    assert total_stops > 20  # heuristic Bull-vs-Fae spends ~100+ stops over 30 games


def _mirror_win_counts(games: int) -> collections.Counter[str]:
    wins: collections.Counter[str] = collections.Counter()
    for seed in range(games):
        eng = Engine(
            make_deck("A", bull()),
            make_deck("B", bull()),
            RandomPolicy(),
            RandomPolicy(),
            seed=seed,
            created="x",
        )
        wins[eng.play().winner] += 1
    return wins


def test_mirror_match_is_roughly_fair() -> None:
    """Identical competitors both sides -> neither seat has a real edge (§11)."""
    wins = _mirror_win_counts(GAMES)
    rate_a = wins["A"] / GAMES
    assert 0.40 <= rate_a <= 0.60, wins


def test_no_draws_in_mirror_batch() -> None:
    """Every game resolves (finish or count-out); the turn cap is never hit."""
    wins = _mirror_win_counts(GAMES)
    assert wins["draw"] == 0, wins


def test_turn_roll_is_fair_against_closed_form() -> None:
    """Both competitors' skills are {5..10}, so a single roll-off is 50/50 by
    symmetry; the Monte-Carlo winner rate converges there (§11)."""
    winners: collections.Counter[str] = collections.Counter()
    for seed in range(ROLLS):
        eng = Engine(
            make_deck("A", bull()),
            make_deck("B", fae()),
            RandomPolicy(),
            RandomPolicy(),
            seed=seed,
            created="x",
        )
        eng.setup()
        eng.state.turn_no = 1
        winners[eng._roll_off()] += 1
    rate_a = winners["A"] / ROLLS
    assert 0.45 <= rate_a <= 0.55, winners


def test_same_seed_is_byte_identical_and_replays() -> None:
    def run() -> Engine:
        eng = Engine(
            make_deck("A", bull()),
            make_deck("B", fae()),
            RandomPolicy(),
            RandomPolicy(),
            seed=2024,
            created="2026-07-14",
        )
        eng.play()
        return eng

    first, second = run(), run()
    assert first.state.log.to_lines() == second.state.log.to_lines()
    assert matches(first.state.log, second.state.log)
    assert first.result == second.result


# -- turn-roll gimmick layer (todo #17 / #31) --------------------------------


def _forced_roll_off(a_val: int, b_val: int, comp_a: Any, comp_b: Any) -> Engine:
    """An engine whose next roll-off is forced to ``(A=a_val, B=b_val)`` (same
    skill both sides), so a single deterministic turn roll can be inspected."""
    eng = Engine(
        make_deck("A", comp_a), make_deck("B", comp_b), RandomPolicy(), RandomPolicy(), seed=1
    )
    eng.setup()
    eng.state.turn_no = 1
    rolls = {"A": (Skill.POWER, a_val), "B": (Skill.POWER, b_val)}
    eng._roll_for = lambda key, use_pending: rolls[key]  # type: ignore[method-assign]
    return eng


@pytest.mark.parametrize(
    ("bull_val", "expected_bonus"),
    [(7, 1), (6, 2), (5, 3), (10, 0), (8, 0)],  # 3/4/5+ less -> +1/+2/+3; higher or <3 -> none
)
def test_bull_comeback_scales_with_the_roll_gap(bull_val: int, expected_bonus: int) -> None:
    """The Bull's gimmick reads "when your roll is exactly 3 less than your target's
    ...": +1/+2/+3 for 3/4/5+ less, and nothing when it rolled higher (SUPERSHOW
    §2). It is roll-value keyed, so it fires regardless of who won the turn."""
    eng = _forced_roll_off(bull_val, 10, bull_gimmick(), vanilla())
    eng._turn_roll()
    assert eng.state.players["A"].pending_roll_mods["next"] == expected_bonus


def test_comeback_boost_lands_on_the_next_roll() -> None:
    # +1 pending after rolling 3 less is consumed by (added to) the following roll.
    eng = _forced_roll_off(7, 10, bull_gimmick(), vanilla())
    eng._turn_roll()
    assert eng.state.players["A"].pending_roll_mods["next"] == 1
    eng._roll_for = lambda key, use_pending: (  # next roll-off: A base 9, B base 9
        Skill.POWER,
        9 + (eng.state.players[key].pending_roll_mods["this"] if use_pending else 0),
    )
    eng._turn_roll()  # A's applied roll is 9+1=10 > 9 -> A wins the next turn
    assert eng.state.active == "A"


def test_lowest_wins_flips_the_roll_off() -> None:
    """Fae Dragon's gimmick makes the LOWEST roll win the turn (global — it flips
    for both sides). The Bull's higher roll, normally a win, now loses."""
    assert _forced_roll_off(8, 5, bull_gimmick(), vanilla())._roll_off() == "A"  # highest wins
    assert _forced_roll_off(8, 5, bull_gimmick(), fae_gimmick())._roll_off() == "B"  # lowest wins


def test_blanking_fae_restores_highest_wins() -> None:
    # Lowest-wins is a WHILE_GIMMICK_ACTIVE passive; blanking Fae drops it.
    eng = _forced_roll_off(8, 5, bull_gimmick(), fae_gimmick())
    eng.state.players["B"].gimmick_blanked = True
    assert eng._roll_off() == "A"  # highest wins again


def _turn_roll_winrate(comp_a: Any, comp_b: Any, n: int, seed: int = 11) -> float:
    """A's turn-roll win rate over an ``n``-roll stream, carrying pending comeback
    bonuses across roll-offs (decks refilled so bumps never deplete the stream)."""
    eng = Engine(
        make_deck("A", comp_a), make_deck("B", comp_b), RandomPolicy(), RandomPolicy(), seed=seed
    )
    eng.setup()
    wins: collections.Counter[str] = collections.Counter()
    for i in range(n):
        for key in ("A", "B"):
            player = eng.state.players[key]
            player.hand = []
            player.deck = [_FILLER] * 8  # keep bumps/count-out from ending the stream
        eng.state.turn_no = i + 1
        wins[eng._turn_roll()] += 1
    return wins["A"] / n


def test_turn_roll_parity_bull_vs_vanilla() -> None:
    """Bull's comeback vs a vanilla line reproduces tournament_turnsim's ~54.1% (§11)."""
    rate = _turn_roll_winrate(bull_gimmick(), vanilla(), TURN_ROLL_STREAM)
    assert abs(rate - REF_BULL_VS_VANILLA) < PARITY_TOL, rate


def test_turn_roll_parity_bull_vs_fae() -> None:
    """Vs Fae's lowest-wins the comeback backfires (a roll boost loses under lowest-
    wins), pulling the Bull to the reference's ~45.9% (§11)."""
    rate = _turn_roll_winrate(bull_gimmick(), fae_gimmick(), TURN_ROLL_STREAM)
    assert abs(rate - REF_BULL_VS_FAE) < PARITY_TOL, rate


def test_gimmick_mirror_is_symmetric() -> None:
    # Identical gimmicked competitors: no seat has an edge (bumps add variance,
    # not bias), so the stream sits on ~50%.
    rate = _turn_roll_winrate(bull_gimmick(), bull_gimmick(), TURN_ROLL_STREAM)
    assert 0.48 <= rate <= 0.52, rate


def test_gimmick_matchup_report_is_deterministic() -> None:
    """Full gimmicked games over a seed range give a byte-identical MatchupReport —
    the turn-roll layer keeps the batch reproducible (§11)."""
    from srg_sim.analysis import Matchup, MatchupReport, run_batch, seed_range

    matchup = Matchup(make_deck("A", bull_gimmick()), make_deck("B", fae_gimmick()))
    first = MatchupReport.from_outcomes(run_batch(matchup, seed_range(30), keep_logs=True))
    second = MatchupReport.from_outcomes(run_batch(matchup, seed_range(30), keep_logs=True))
    assert first.to_dict() == second.to_dict()


# -- opt-in cross-check against the fae_comp reference (skipped when absent) ---

_REF_PATH = Path("/home/brandon/fae_comp/tournament_turnsim.py")


def _load_reference() -> Any:
    if not _REF_PATH.exists():
        pytest.skip("fae_comp/tournament_turnsim.py not present (opt-in parity guard)")
    spec = importlib.util.spec_from_file_location("tournament_turnsim", _REF_PATH)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_reference_turnsim_still_reports_the_golden_numbers() -> None:
    """Guard the source of truth: tournament_turnsim's own self-checks still land on
    54.1 / 45.9 (so our baked targets can't silently drift from the reference)."""
    ts = _load_reference()
    bull_spec = {"stats": dict(BULL_STATS_REF), "comeback": "bull"}
    fae_spec = {"stats": dict(FAE_STATS_REF), "lowest_wins": True}
    vanilla_spec = {"stats": dict(VANILLA_STATS_REF)}
    assert ts.run(bull_spec, vanilla_spec, n=200_000) == pytest.approx(
        REF_BULL_VS_VANILLA, abs=0.01
    )
    assert ts.run(bull_spec, fae_spec, n=200_000) == pytest.approx(REF_BULL_VS_FAE, abs=0.01)


BULL_STATS_REF = {
    "Power": 10,
    "Technique": 6,
    "Agility": 5,
    "Submission": 8,
    "Grapple": 9,
    "Strike": 7,
}
FAE_STATS_REF = {
    "Power": 10,
    "Technique": 7,
    "Agility": 6,
    "Submission": 9,
    "Grapple": 5,
    "Strike": 8,
}
VANILLA_STATS_REF = {
    "Power": 10,
    "Technique": 8,
    "Agility": 9,
    "Submission": 7,
    "Grapple": 6,
    "Strike": 5,
}
