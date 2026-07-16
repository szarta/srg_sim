"""Turn-roll win odds between two competitors (exact when possible, else sim).

A turn roll is each side rolling one of six equiprobable skill faces; higher wins,
ties bump (redraw + re-roll). When neither competitor has an effect that touches a
roll, the odds are an **exact** 6x6 face enumeration. When either does (a lowest-
wins flip, a comeback ``ModifyRoll``, a persistent skill buff, a bump-punish...),
we fall back to a seeded **Monte-Carlo** over the real engine roll — the same
``Engine._turn_roll`` the validation parity tests drive — so every gimmick is
honored because it *is* the engine, never a re-derivation.
"""

from __future__ import annotations

from dataclasses import dataclass
from itertools import product

from srg_sim import effects as fx
from srg_sim.analysis import wilson_interval
from srg_sim.cards import AtkType, Card, Competitor, Deck, EntranceCard, PlayOrder, Skill
from srg_sim.engine import Engine
from srg_sim.policy import RandomPolicy

# Effect shapes that make a turn roll depend on more than the base stat line, so
# the exact enumeration no longer applies and the engine MC must decide it.
_ROLL_ACTIONS = (fx.ModifyRoll, fx.BuffSkill, fx.LowestRollWins, fx.WinTie, fx.Bump, fx.Reroll)
_ROLL_TRIGGERS = (fx.OnRoll, fx.OnLoseTurn, fx.OnWinTurn, fx.OnBump)


@dataclass(frozen=True)
class TurnOdds:
    """A competitor's turn-roll win split, tagged with how it was computed."""

    win_a: float
    win_b: float
    tie_reroll_prob: float  # P(a face pair ties -> bump); 0.0 for the MC path
    method: str  # "exact" | "mc"
    n: int | None  # MC roll count, else None
    ci_a: tuple[float, float] | None = None  # Wilson 95% CI on win_a (MC only)


def turn_odds(
    comp_a: Competitor, comp_b: Competitor, *, mc_games: int = 50_000, seed: int = 11
) -> TurnOdds:
    """A's / B's chance to win a turn roll. Exact when both are roll-vanilla."""
    if _touches_roll(comp_a) or _touches_roll(comp_b):
        return _mc_turn_odds(comp_a, comp_b, mc_games, seed)
    return _exact_turn_odds(comp_a.stats.to_dict(), comp_b.stats.to_dict())


def _touches_roll(comp: Competitor) -> bool:
    for eff in comp.effects:
        if isinstance(eff.trigger, _ROLL_TRIGGERS):
            return True
        if any(isinstance(a, _ROLL_ACTIONS) for a in eff.actions):
            return True
    return False


def _exact_turn_odds(a: dict[str, int], b: dict[str, int]) -> TurnOdds:
    faces_a = [a[s.value] for s in Skill]
    faces_b = [b[s.value] for s in Skill]
    aw = bw = tie = 0
    for va, vb in product(faces_a, faces_b):  # 36 equiprobable face pairs
        if va == vb:
            tie += 1
        elif va > vb:
            aw += 1
        else:
            bw += 1
    decided = aw + bw  # ties re-roll into a fresh symmetric sub-game -> normalize them out
    return TurnOdds(aw / decided, bw / decided, tie / 36, "exact", None)


def _mc_turn_odds(comp_a: Competitor, comp_b: Competitor, n: int, seed: int) -> TurnOdds:
    engine = Engine(
        _minimal_deck("A", comp_a),
        _minimal_deck("B", comp_b),
        RandomPolicy(),
        RandomPolicy(),
        seed=seed,
    )
    engine.setup()
    filler = _filler_card()
    wins = {"A": 0, "B": 0}
    for i in range(n):
        for key in ("A", "B"):  # refill so bumps/count-out never end the roll stream
            player = engine.state.players[key]
            player.hand = []
            player.deck = [filler] * 8
        engine.state.turn_no = i + 1
        wins[engine._turn_roll()] += 1  # noqa: SLF001 (report drives the engine's roll)
        if engine.state.log is not None:
            engine.state.log.events.clear()  # bound memory over a long stream
    total = wins["A"] + wins["B"]
    lo, hi = wilson_interval(wins["A"], total)
    return TurnOdds(wins["A"] / total, wins["B"] / total, 0.0, "mc", n, (lo, hi))


def _filler_card() -> Card:
    """An inert main card used only to keep a roll stream's deck non-empty."""
    return Card(
        db_uuid="rep-filler",
        name="filler",
        number=1,
        atk_type=AtkType.STRIKE,
        play_order=PlayOrder.LEAD,
    )


def _minimal_deck(side: str, comp: Competitor) -> Deck:
    """A throwaway legal-enough deck around ``comp`` for the roll-only MC engine."""
    entrance = EntranceCard(db_uuid=f"rep-ent-{side}", name=f"{side} Entrance")
    return Deck(competitor=comp, entrance=entrance, cards=tuple(_filler_card() for _ in range(30)))
