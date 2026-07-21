"""Turn loop, effect executor, stop resolution, finish sequence (DESIGN.md §6).

The engine plays two :class:`~srg_sim.cards.Deck` s against two
:class:`~srg_sim.policy.Policy` policies under one seed and produces a complete
JSONL :class:`~srg_sim.gamelog.GameLog` plus a :class:`GameResult`. Everything
non-deterministic flows through the shared :class:`~srg_sim.rng.SeededRNG`, so
``Engine(...).play()`` is a pure function of ``(decks, policies, seed)`` —
re-running reproduces a byte-identical log (DESIGN.md §8 replay).

**Turn structure** (DESIGN.md §6). On a won turn the active player draws 1 then
plays **one** card advancing the order-only chain (a Lead is always playable; a
Follow Up needs a Lead in play; a Finish needs a Follow Up in play) or passes and
buries 1. The in-play board **persists across turns** (both sides) and clears only
on a breakout, which discards every in-play card on both sides and bumps the Crowd
Meter. Rolls and breakouts use **actual seeded draws** (a face is drawn, its value is the
derived stat), sharing the exact per-face rules ported into :mod:`srg_sim.finish`.
**Stops are text-driven**: a hand card can stop an attack iff one of its parsed
``Stop`` effects matches the attack's order/type and that effect's condition holds
(evaluated by :mod:`srg_sim.conditions` — so skill stops, see-1, and crowd-meter
gates all fall out). A card with no Stop effect cannot stop. The executor applies
a focused action set; any effect it cannot apply — an ``Unsupported`` node or an
unhandled action — is emitted as an ``unsupported`` log event, never dropped.
"""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass, replace
from typing import TYPE_CHECKING, Any

from srg_sim import conditions
from srg_sim import effects as fx
from srg_sim import gamelog as gl
from srg_sim.cards import AtkType, Card, Deck, PlayOrder, Skill
from srg_sim.finish import is_auto_success, stat_breaks_out
from srg_sim.rng import SeededRNG
from srg_sim.state import GameState, PendingText, PlayerState, TimedBuff

if TYPE_CHECKING:
    from srg_sim.policy import Option, Policy

OPENING_HAND = 3
HAND_CAP = 10
BREAKOUT_ATTEMPTS = 3
TURN_CAP = 400
MAX_TIE_REROLLS = 64

# RPS: a beats b. Strike ▷ Grapple ▷ Submission ▷ Strike (DESIGN.md §2). Stops are
# text-driven (a card's parsed Stop effects), so this is a validation/analysis
# utility — the RPS relationship is baked into each card's printed stop text.
_BEATS = {
    AtkType.STRIKE: AtkType.GRAPPLE,
    AtkType.GRAPPLE: AtkType.SUBMISSION,
    AtkType.SUBMISSION: AtkType.STRIKE,
}


def beats(attacker: AtkType, defender: AtkType) -> bool:
    """True iff a ``defender``-type card stops an ``attacker``-type attack (RPS)."""
    return _BEATS.get(defender) is attacker


@dataclass(frozen=True)
class GameResult:
    """The match outcome (DESIGN.md §6). ``winner`` is a player key or ``"draw"``."""

    winner: str
    reason: str  # finish | count_out | disqualification | pinfall | turn_cap
    turns: int


class Engine:
    """Plays a single match to completion (DESIGN.md §6 turn loop)."""

    def __init__(
        self,
        deck_a: Deck,
        deck_b: Deck,
        policy_a: Policy,
        policy_b: Policy,
        seed: int,
        created: str = "",
        kind: str = "sim",
    ) -> None:
        rng = SeededRNG(seed)
        self.decks = {"A": deck_a, "B": deck_b}
        self.policies: dict[str, Policy] = {"A": policy_a, "B": policy_b}
        self._kind = kind  # "sim" | "real" (a human took at least one decision)
        self.state = GameState(players=self._build_players(), rng=rng)
        self.state.log = gl.GameLog(header=self._header(seed, created))
        self.result: GameResult | None = None
        self._pending_loss: tuple[str, str] | None = None
        # Per-player context of the most recent roll-off (rolled skill + margin),
        # so turn-roll gimmicks (OnWinTurn/OnLoseTurn) can gate on RollGap*/RollWasSkill.
        self._roll_ctx: dict[str, conditions.RollContext] = {}
        # Whether this turn's roll-off involved a bump — read by the finish sequence
        # for "if you bumped on the last turn roll, double these bonuses" (T-Virus).
        self._turn_bumped = False
        # The clause currently resolving, read only by a TIMED BuffSkill for its
        # stacking identity (set by `_apply_actions`).
        self._clause = ""
        # db_uuid of the card currently being stopped, set for the duration of
        # _apply_stop so BlankStoppedText knows its referent. Never serialized.
        self._stopped_card: str | None = None
        # The card whose hit is currently resolving, for a per_excludes_trigger count.
        self._hit_card: Card | None = None

    # -- setup -------------------------------------------------------------

    def _build_players(self) -> dict[str, PlayerState]:
        return {k: self._build_player(k) for k in ("A", "B")}

    def _build_player(self, key: str) -> PlayerState:
        deck = self.decks[key]
        return PlayerState(
            competitor=deck.competitor, entrance=deck.entrance, deck=list(deck.cards)
        )

    def _header(self, seed: int, created: str) -> gl.Header:
        return gl.Header(
            seed=seed,
            kind=self._kind,
            created=created,
            players={k: self._player_info(k) for k in ("A", "B")},
        )

    def _player_info(self, key: str) -> gl.PlayerInfo:
        deck = self.decks[key]
        return gl.PlayerInfo(
            competitor=deck.competitor.name,
            entrance=deck.entrance.name,
            deck=[c.db_uuid for c in deck.cards],
            policy=self.policies[key].name,
        )

    def setup(self) -> None:
        """Match setup: StartOfMatch effects, shuffle, opening hands.

        The first-turn redraw is NOT done here — it belongs to each player's own
        first won turn (DESIGN.md §6, srg-rules-confirmed), fired from the turn loop.
        """
        for key in ("A", "B"):
            self._run_effects(self._standing_effects(key), fx.StartOfMatch, key)
        for key in ("A", "B"):
            self.state.rng.shuffle(self.state.players[key].deck)
        for key in ("A", "B"):
            self._draw(key, OPENING_HAND)

    def _first_turn_option(self, key: str) -> None:
        """Offer the first-turn redraw once per player, on the first won turn they
        would take an action (DESIGN.md §6). Marked spent whether or not it fires,
        so a player who bumps/loses the early rolls still gets it exactly once."""
        player = self.state.players[key]
        if player.flags.get("had_first_turn"):
            return
        player.flags["had_first_turn"] = True
        self._mulligan(key)

    def _mulligan(self, key: str) -> None:
        # First-turn redraw (DESIGN.md §6): only with NO Leads in hand, a player MAY
        # reveal the whole hand, bury it to the bottom of the deck IN AN ORDER THEY
        # CHOOSE, then draw UP TO that many. With a Lead in hand it is not offered.
        player = self.state.players[key]
        if not player.hand or any(c.play_order is PlayOrder.LEAD for c in player.hand):
            return
        legal: list[Option] = [{"kind": "redraw"}, {"kind": "keep"}]
        if self._decide("mulligan", key, legal)["kind"] != "redraw":
            return
        revealed = list(player.hand)
        player.hand.clear()
        ordered = self._order_bury(key, revealed)  # player picks the bury order
        player.deck.extend(ordered)  # to the bottom of the deck, in that order
        self._log(
            gl.Bury(
                t=self.state.turn_no,
                player=key,
                cards=[c.db_uuid for c in ordered],
                source="hand",
                hidden=False,  # the hand was REVEALED, so the moved cards are public
            )
        )
        self._draw(key, self._mulligan_draw_count(key, len(revealed)))  # draw UP TO N

    def _order_bury(self, key: str, cards: list[Card]) -> list[Card]:
        """Return ``cards`` in the owner's chosen bury order (last card forced)."""
        remaining = list(cards)
        ordered: list[Card] = []
        while len(remaining) > 1:
            chosen = self._decide(
                "mulligan_bury", key, [self._discard_option(c) for c in remaining]
            )
            card = next(c for c in remaining if c.db_uuid == chosen["card"])
            remaining.remove(card)
            ordered.append(card)
        ordered.extend(remaining)
        return ordered

    def _mulligan_draw_count(self, key: str, n: int) -> int:
        """How many to redraw: up to ``n`` (default policy takes the max — listed first)."""
        legal: list[Option] = [{"kind": "draw", "n": i} for i in range(n, -1, -1)]
        return int(self._decide("mulligan_draw", key, legal)["n"])

    # -- main loop ---------------------------------------------------------

    def play(self) -> GameResult:
        """Run the match to a result and return it (log is on ``self.state.log``)."""
        self.setup()
        while self.result is None and self.state.turn_no < TURN_CAP:
            self._turn()
        if self.result is None:
            self.result = GameResult("draw", "turn_cap", self.state.turn_no)
        self._log(self._result_event())
        return self.result

    def _result_event(self) -> gl.Result:
        r = self.result
        assert r is not None
        return gl.Result(t=self.state.turn_no, winner=r.winner, reason=r.reason, turns=r.turns)

    def _turn(self) -> None:
        self.state.turn_no += 1
        self._clear_turn_freq()
        for player in self.state.players.values():
            player.flags.pop("extra_plays", 0)  # "additional card this turn" is per-turn
            # Promote a "re-roll your next turn roll" grant to this turn (SET, not
            # accumulate); an unused grant expires.
            player.reroll_grants["this"] = player.reroll_grants["next"]
            player.reroll_grants["next"] = 0
        self._sweep_end_of_turn()
        winner = self._turn_roll()
        self._sweep_next_turn_buffs(winner)
        if self._ended() or not self._draw_for_turn(winner):
            return
        self._first_turn_option(winner)  # the once-per-player first-turn redraw (§6)
        self._run_start_of_turn(winner)  # "once during your turn" gimmicks (Candyman Dan)
        self._run_opponent_turn(self.state.opponent_of(winner))  # "once during your opponent's turn"
        if self._ended():
            return
        self._take_turn_action(winner)  # play ONE card (or pass+bury); the board persists
        while not self._ended() and self._consume_extra_play(winner):
            self._take_turn_action(winner)  # a PlayExtraCard granted another action

    def _run_start_of_turn(self, key: str) -> None:
        """Fire the active player's ``StartOfTurn`` gimmicks — "once during your turn,
        you may …" (Candyman Dan). Offered right after the turn draw, before the play
        action. Previously a dead trigger; no parsed card / other override emits it."""
        self._run_effects(self._standing_effects(key), fx.StartOfTurn, key)

    def _run_opponent_turn(self, key: str) -> None:
        """Fire the NON-active player's ``DuringOpponentTurn`` gimmicks — "once during
        your opponent's turn, you may …" (Memes Dealer V1), the mirror of
        :meth:`_run_start_of_turn`. Previously unused; no parsed card / override emits it."""
        self._run_effects(self._standing_effects(key), fx.DuringOpponentTurn, key)

    def _consume_extra_play(self, key: str) -> bool:
        """Spend one pending "additional card this turn" grant, if any."""
        flags = self.state.players[key].flags
        if flags.get("extra_plays", 0) <= 0:
            return False
        flags["extra_plays"] -= 1
        return True

    def _turn_roll(self) -> str:
        """Resolve the roll-off and fire the turn-roll gimmicks (DESIGN.md §6/§11).

        Split out from :meth:`_turn` so the gimmick layer — Bull's gap-based
        comeback (``OnRoll`` + ``RollGap*`` -> ``ModifyRoll(NEXT)``) and Fae's
        lowest-wins flip — can be driven and measured without the draw/play tail.
        """
        winner = self._roll_off()
        self.state.active = winner
        loser = self.state.opponent_of(winner)
        ctx = self._roll_ctx
        self._run_effects(self._standing_effects(winner), fx.OnWinTurn, winner, ctx[winner])
        self._run_effects(self._standing_effects(loser), fx.OnLoseTurn, loser, ctx[loser])
        # OnRoll is outcome-agnostic (fires on each side's roll), so it fires for
        # both players regardless of who won — the Bull's "N less than target" comeback.
        # Same srgpc ordering rule for the post-roll OnRoll gimmicks.
        for key in self._roll_order(
            (self._roll_ctx.get("A").value if self._roll_ctx.get("A") else None) or 0,
            (self._roll_ctx.get("B").value if self._roll_ctx.get("B") else None) or 0,
        ):
            self._run_on_roll(key)
            self._run_on_rolled_all(key)
        self.state.last_roll_winner = winner  # "last turn roll" for next turn (Dunn)
        return winner

    def _run_on_bump(self) -> None:
        """Fire both players' ``OnBump`` effects for a bump just taken (both sides
        bump on a tie). A once-per-turn frequency guard keeps a bump-punish gimmick
        firing only once even when a roll ties repeatedly in one turn."""
        for key in ("A", "B"):
            self._run_effects(self._standing_effects(key), fx.OnBump, key)

    @staticmethod
    def _roll_order(va: int, vb: int) -> tuple[str, str]:
        """Resolution order for gimmicks that trigger DURING a turn roll.

        srgpc.net: "If two gimmicks would both trigger during a turn roll, the player
        with the higher turn roll must resolve their effect first." Evaluated against
        the roll values as they stand entering each stage. On an exact tie the order
        is undefined by the rules, so the stable A-then-B order is kept (a tie bumps,
        and no gimmick ordering is decided by it)."""
        return ("B", "A") if vb > va else ("A", "B")

    def _run_on_roll(self, key: str) -> None:
        """Fire ``key``'s ``OnRoll`` effects for the deciding turn roll: matched by
        the roller's skill (``None`` = any) and gated by the roller's roll context."""
        opp = self.state.opponent_of(key)
        for eff in self._standing_effects(key):
            trig = eff.trigger
            if not isinstance(trig, fx.OnRoll):
                continue
            ctx = self._roll_ctx[key if trig.who is fx.Who.SELF else opp]
            if trig.skill is None or ctx.skill is trig.skill:
                self._fire_if_ready(eff, key, ctx)

    def _run_on_rolled_all(self, key: str) -> None:
        """Accumulate ``key``'s turn-roll skills for its ``OnRolledAll`` gimmicks. Each
        records the rolled skill in a per-effect bitmask (in ``freq_counters``, so it
        persists across turns — not a ``turn:`` guard); when a gimmick has seen EVERY
        required skill, it fires and its accumulator resets (General Lee Wong V2)."""
        opp = self.state.opponent_of(key)
        for eff in self._standing_effects(key):
            trig = eff.trigger
            if not isinstance(trig, fx.OnRolledAll):
                continue
            ctx = self._roll_ctx[key if trig.who is fx.Who.SELF else opp]
            if ctx.skill is None or ctx.skill not in trig.skills:
                continue
            counters = self.state.players[key].freq_counters
            k = _rolled_set_key(eff)
            counters[k] = counters.get(k, 0) | _skill_bit(ctx.skill)
            want = 0
            for s in trig.skills:
                want |= _skill_bit(s)
            if (counters.get(k, 0) & want) == want:
                counters.pop(k, None)  # reset the set — "each time"
                self._fire_if_ready(eff, key, ctx)
            if self._ended():
                return

    def _run_on_finish_roll(self, finisher: str, skill: Skill, value: int) -> None:
        """Fire ``OnFinishRoll`` gimmicks for ``finisher``'s Finish roll. A separate
        trigger from the turn-roll ``OnRoll``, so no existing gimmick fires on a Finish
        roll. BOTH players are scanned: ``who=SELF`` fires for the finisher and
        ``who=OPP`` for the non-finisher (matching ``OnRoll``'s convention). The Finish
        roll does not populate ``_roll_ctx``, so a local context carries skill/value."""
        ctx = conditions.RollContext(skill=skill, value=value)
        for owner in ("A", "B"):
            opp = self.state.opponent_of(owner)
            for eff in self._standing_effects(owner):
                trig = eff.trigger
                if not isinstance(trig, fx.OnFinishRoll):
                    continue
                target = owner if trig.who is fx.Who.SELF else opp
                if target != finisher:
                    continue
                if trig.skill is None or trig.skill is skill:
                    self._fire_if_ready(eff, owner, ctx)
                if self._ended():
                    return

    # -- roll-off ----------------------------------------------------------

    def _roll_off(self) -> str:
        lowest = self._lowest_wins()
        self._promote_pending()  # last turn's `when=NEXT` mods become THIS roll's (#50)
        sa, va = self._roll_for("A", use_pending=True)
        sb, vb = self._roll_for("B", use_pending=True)
        # Switch-rolled-skill (Scott Prime): "you may switch the rolled skill to Power"
        # — offered before boosts/mods so they land on the switched skill.
        sa, va, sb, vb = self._offer_switches(sa, va, sb, vb)
        # In-roll boosts (Soborno): after the skill is known, before the winner is
        # decided, a player may pay a cost for +delta to THIS roll — so it can flip
        # the outcome or break a tie. A no-op for competitors without such a gimmick.
        for owner in self._roll_order(va, vb):
            if owner == "A":
                va = self._offer_roll_boost("A", sa, va)
            else:
                vb = self._offer_roll_boost("B", sb, vb)
        va, vb = self._apply_in_roll_mods(sa, va, sb, vb)  # Tomato: roll-skill debuff
        sa, va, sb, vb = self._offer_rerolls(sa, va, sb, vb)  # Dunn/Jay White: optional re-roll
        self._consume_pending()
        bumps = 0
        while bumps < MAX_TIE_REROLLS:
            # Elective same-skill bump (Mastermind's "Ringside Ruckus" entrance): both
            # rolled the SAME skill but different values, so the owner MAY spend a
            # per-match charge to bump instead of resolving. A value tie bumps for free
            # below, so this only adds the value-differs case.
            if va != vb and sa == sb:
                owner = self._elective_bump_owner()
                if owner is not None and self._elect_bump(owner, va, vb):
                    sa, va, sb, vb, bumps = self._do_bump(bumps)
                    continue
            if va != vb:
                break  # a decided roll: no value tie and no elected bump
            forced = self._tie_winner()
            if forced is not None:
                self._record_roll_ctx(sa, va, sb, vb)
                self._turn_bumped = bumps > 0
                self.state.last_turn_bumped = bumps > 0  # read by the NEXT turn (Mack-a-Tack)
                self._log(gl.TurnResult(t=self.state.turn_no, winner=forced, tie_bumps=bumps))
                return forced
            # Would-bump replacement (Rey Zerblade): on a tie, before bumping, a player
            # may pay a cost for +delta to THIS roll *instead* of the bump. If that
            # breaks the tie, the bump is skipped entirely.
            # Tie-only path (va == vb), so _roll_order's documented tie fallback
            # (A then B) already applies; kept explicit for clarity.
            va = self._offer_roll_boost("A", sa, va, on_bump=True)
            vb = self._offer_roll_boost("B", sb, vb, on_bump=True)
            if va != vb:
                break
            sa, va, sb, vb, bumps = self._do_bump(bumps)
        winner = self._roll_winner(va, vb, lowest)
        self._record_roll_ctx(sa, va, sb, vb)
        self._turn_bumped = bumps > 0
        self.state.last_turn_bumped = bumps > 0  # read by the NEXT turn (Mack-a-Tack)
        self._log(gl.TurnResult(t=self.state.turn_no, winner=winner, tie_bumps=bumps))
        return winner

    def _bump_draw(self, key: str) -> None:
        """A player's bump card: normally draw 1, but if their OPPONENT declares
        ``BumpDrawReplace`` (Mack-a-Tack), discard 1 from hand INSTEAD ("when you bump,
        your opponent discards 1 card instead of drawing")."""
        opp = self.state.opponent_of(key)
        if self._declares_static(opp, fx.BumpDrawReplace):
            self._discard_from_hand(key, 1, False)
        else:
            self._draw(key, 1)

    def _do_bump(self, bumps: int) -> tuple[Skill, int, Skill, int, int]:
        """Perform a bump: both players draw 1 (unless replaced by a discard), fire
        OnBump punishes, and re-roll (pending mods are dropped on a bump re-roll).
        Returns the fresh ``(sa, va, sb, vb, bumps+1)`` for the roll-off loop."""
        self._bump_draw("A")
        self._bump_draw("B")
        bumps += 1
        self._run_on_bump()  # bump-punish gimmicks (Mastermind: opp next roll -2)
        sa, va = self._roll_for("A", use_pending=False)
        sb, vb = self._roll_for("B", use_pending=False)
        sa, va, sb, vb = self._offer_switches(sa, va, sb, vb)  # a bump re-roll is a turn roll too
        va, vb = self._apply_in_roll_mods(sa, va, sb, vb)  # debuff re-rolls too
        sa, va, sb, vb = self._offer_rerolls(sa, va, sb, vb)  # re-roll offered post-bump too
        return sa, va, sb, vb, bumps

    def _offer_rerolls(self, sa: Skill, va: int, sb: Skill, vb: int) -> tuple[Skill, int, Skill, int]:
        """Offer each side its once-per-turn turn-roll re-roll (Dunn, Jay White). A taken
        re-roll REPLACES that side's (skill, value) with a fresh die — kept even if worse
        — and spends the ONCE_PER_TURN charge; declining leaves it for a later roll in the
        same roll-off (initial or any bump). Re-checked each call, so Jay White keys on the
        opponent's *current* roll. Boosts/in-roll mods are not re-applied to a re-rolled
        die (no re-roll competitor also carries those — DESIGN.md §11)."""
        vals = {"A": (sa, va), "B": (sb, vb)}
        ctx = {
            "A": conditions.RollContext(skill=sa, gap=vb - va, value=va, opp_skill=sb),
            "B": conditions.RollContext(skill=sb, gap=va - vb, value=vb, opp_skill=sa),
        }
        # Each side may spend a re-roll; the target die (own, the opponent's, or a
        # chosen player's) is re-rolled in place. Higher roll resolves first (srgpc).
        for owner in self._roll_order(va, vb):
            target = self._offer_reroll(owner, ctx[owner], ctx[self.state.opponent_of(owner)])
            if target is not None:
                skill, value = self._roll_for(target, use_pending=False)
                self._log_effect(owner, "Reroll", target, {"skill": skill.value, "value": value})
                vals[target] = (skill, value)
        return vals["A"][0], vals["A"][1], vals["B"][0], vals["B"][1]

    def _offer_switches(self, sa: Skill, va: int, sb: Skill, vb: int) -> tuple[Skill, int, Skill, int]:
        """Offer each side its "switch the rolled skill" option (Scott Prime). A taken
        switch replaces that side's rolled (skill, value) — the die keeps its roll mods
        (value recomputed on the new skill's stat). Offered at every turn-roll point
        (initial roll + each bump re-roll), mirroring ``_offer_rerolls``."""
        vals = {"A": (sa, va), "B": (sb, vb)}
        for owner in self._roll_order(va, vb):
            skill, value = vals[owner]
            switched = self._offer_switch(owner, skill, value)
            if switched is not None:
                vals[owner] = switched
        return vals["A"][0], vals["A"][1], vals["B"][0], vals["B"][1]

    def _offer_switch(self, owner: str, skill: Skill, value: int) -> tuple[Skill, int] | None:
        """``owner``'s turn-roll switch: if a standing ``SwitchRolledSkill`` fires for the
        rolled ``skill``, recompute the value on the new skill (``value`` minus the old
        skill's stat plus the new one's, preserving roll mods) and log it."""
        to = self._find_switch(owner, skill)
        if to is None:
            return None
        nv = value - self._stat(owner, skill) + self._stat(owner, to)
        self._log_effect(owner, "SwitchRolledSkill", owner, {"from": skill.value, "to": to.value, "value": nv})
        return to, nv

    def _find_switch(self, owner: str, skill: Skill) -> Skill | None:
        """The first standing ``SwitchRolledSkill`` effect whose ``from_skill`` matches the
        rolled ``skill``, whose gate holds, and whose optional offer is taken; returns its
        ``to`` skill, or ``None``. Shared by the turn roll-off and the Finish roll."""
        for eff in self._standing_effects(owner):
            switch = next((a for a in eff.actions if isinstance(a, fx.SwitchRolledSkill)), None)
            if switch is None or switch.from_skill is not skill:
                continue
            ctx = conditions.RollContext(skill=skill, gap=None, value=self._stat(owner, skill))
            if not (self._may_fire(eff, owner) and conditions.holds(eff.condition, self.state, owner, ctx)):
                continue
            if eff.optional and not self._take_optional(eff, owner):
                continue  # declined "you may switch"
            self._mark_fired(eff, owner)
            return switch.to
        return None

    def _offer_reroll(
        self, owner: str, own_ctx: conditions.RollContext, opp_ctx: conditions.RollContext
    ) -> str | None:
        """``owner``'s re-roll offer: the first standing ``Reroll`` effect whose gate holds
        and whose charge is unspent is offered; returns the KEY of the player whose die
        should be re-rolled (own / opponent / a chosen player), or ``None`` if none fires.
        The gate reads the opponent's roll for an ``InRoll(who=OPP)`` trigger (Jay White),
        else the owner's (Reverend "when you roll …")."""
        for eff in self._standing_effects(owner):
            # Only a THIS re-roll is offered structurally; a NEXT re-roll is a deferred
            # grant (handled by _act_reroll + reroll_grants), not fired here.
            reroll = next(
                (a for a in eff.actions if isinstance(a, fx.Reroll) and a.when is fx.RollWhen.THIS),
                None,
            )
            if reroll is None:
                continue
            gate_ctx = (
                opp_ctx
                if isinstance(eff.trigger, fx.InRoll) and eff.trigger.who is fx.Who.OPP
                else own_ctx
            )
            if not (self._may_fire(eff, owner) and conditions.holds(eff.condition, self.state, owner, gate_ctx)):
                continue
            # A costed re-roll (Mr. Hyde) is offered only while the owner can pay it —
            # an in-play card matching ``cost`` to shuffle away. Unaffordable => not
            # offered, and the frequency charge is left unspent.
            if reroll.cost is not None and not self._has_in_play(owner, reroll.cost):
                continue
            if eff.optional and not self._take_optional(eff, owner):
                continue  # declined "you may" — charge left for a later roll
            self._mark_fired(eff, owner)
            if reroll.cost is not None:
                self._pay_reroll_cost(owner, reroll.cost)
            if reroll.choose:
                return self._decide_reroll_target(owner)
            if reroll.who is fx.Who.OPP:
                return self.state.opponent_of(owner)
            return owner
        # A granted "re-roll your next turn roll" (King Brian Cage): a one-shot
        # optional self-re-roll, usable at any roll point until spent.
        if self.state.players[owner].reroll_grants["this"] > 0 and self._offer_yes_no(owner):
            self.state.players[owner].reroll_grants["this"] -= 1
            return owner
        return None

    def _decide_reroll_target(self, owner: str) -> str:
        """"Choose any player to re-roll" (Grim Librarian): the owner picks which side."""
        legal = [{"kind": "reroll_target", "target": "OPP"}, {"kind": "reroll_target", "target": "SELF"}]
        chosen = self._decide("reroll_target", owner, legal)
        return owner if chosen["target"] == "SELF" else self.state.opponent_of(owner)

    def _act_reroll(self, action: fx.Reroll, key: str) -> None:
        """A ``THIS`` re-roll is structural (read in the roll-off) — a no-op here. A
        ``NEXT`` re-roll grants a one-shot for ``key``'s next turn roll (King Brian Cage)."""
        if action.when is fx.RollWhen.NEXT:
            self.state.players[key].reroll_grants["next"] += 1

    def _has_in_play(self, owner: str, filt: fx.CardFilter) -> bool:
        """Whether ``owner`` has any in-play card matching ``filt`` (re-roll cost check)."""
        return any(conditions.card_matches(c, filt) for c in self.state.players[owner].in_play)

    def _pay_reroll_cost(self, owner: str, filt: fx.CardFilter) -> None:
        """Pay a costed re-roll: shuffle the first in-play card matching ``filt`` into
        ``owner``'s deck (Mr. Hyde's "Potion"). Fires ``OnShuffle`` like any shuffle."""
        player = self.state.players[owner]
        card = next((c for c in player.in_play if conditions.card_matches(c, filt)), None)
        if card is None:
            return  # affordability was checked before offering
        player.in_play.remove(card)
        player.deck.append(card)
        self._log(gl.Bury(t=self.state.turn_no, player=owner, cards=[card.db_uuid], source="in_play"))
        self._shuffle_deck(owner)

    def _offer_yes_no(self, key: str) -> bool:
        """A bare optional yes/no offer to ``key`` (no backing effect)."""
        chosen = self._decide("optional", key, [{"kind": "yes"}, {"kind": "no"}])
        return chosen["kind"] == "yes"

    def _elective_bump_owner(self) -> str | None:
        """A player who holds an ``ElectBumpOnSameSkill`` grant with a per-match charge
        still available (else ``None``) — the roll-off consults this on a same-skill,
        value-differs roll to offer an elective bump."""
        for key in ("A", "B"):
            for eff in self._standing_effects(key):
                for a in eff.actions:
                    if isinstance(a, fx.ElectBumpOnSameSkill):
                        used = self.state.players[key].freq_counters.get("match:elect_bump", 0)
                        if used < a.uses:
                            return key
        return None

    def _elect_bump(self, owner: str, va: int, vb: int) -> bool:
        """Offer ``owner`` the elective same-skill bump and spend a charge if taken.
        The options carry a ``losing`` hint (is the owner behind on this roll?) so a
        policy can bump a loss into a re-roll and pass on a win."""
        mine, theirs = (va, vb) if owner == "A" else (vb, va)
        losing = mine < theirs
        legal: list[Option] = [
            {"kind": "yes", "point": "elect_bump", "losing": losing},
            {"kind": "no", "point": "elect_bump", "losing": losing},
        ]
        if self._decide("elect_bump", owner, legal)["kind"] != "yes":
            return False
        fc = self.state.players[owner].freq_counters
        fc["match:elect_bump"] = fc.get("match:elect_bump", 0) + 1
        return True

    def _offer_roll_boost(self, key: str, skill: Skill, value: int, on_bump: bool = False) -> int:
        """Offer ``key``'s in-roll boosts for a roll of ``skill`` and return the (maybe
        boosted) value. Each matching :class:`~srg_sim.effects.OnRollBoost` effect whose
        cost is payable (condition holds) is offered; taking it pays the cost (its
        actions, e.g. a type-matched discard) and adds ``delta`` to this roll. ``on_bump``
        selects which boosts apply: the initial roll offers ``on_bump=False`` boosts
        (Soborno), a would-bump tie offers ``on_bump=True`` ones (Rey Zerblade)."""
        for eff in self._standing_effects(key):
            trig = eff.trigger
            if not isinstance(trig, fx.OnRollBoost):
                continue
            if trig.on_bump is not on_bump:
                continue
            if trig.skill is not None and trig.skill is not skill:
                continue
            if not (self._may_fire(eff, key) and conditions.holds(eff.condition, self.state, key)):
                continue
            if eff.optional and not self._take_optional(eff, key):
                continue
            self._mark_fired(eff, key)
            self._apply_actions(eff, key)  # pay the cost (e.g. discard a matching card)
            value += trig.delta
            self._log_effect(key, "RollBoost", key, {"skill": skill.value, "delta": trig.delta})
        return value

    def _apply_in_roll_mods(self, sa: Skill, va: int, sb: Skill, vb: int) -> tuple[int, int]:
        """Apply automatic in-roll modifiers to the current roll (Tomato Tomato Jr.:
        "when you or your target roll Power, your target's roll is -1"). Each
        :class:`~srg_sim.effects.InRoll` effect whose skill gate matches adds its
        ``ModifyRoll(when=THIS)`` deltas to the named side's value — one action, one
        application, so an ``either``-gated debuff is capped, never doubled."""
        rolled = {"A": sa, "B": sb}
        vals = {"A": va, "B": vb}
        # Roll context for the in-progress roll-off, so a value-gated in-roll modifier
        # (Numer01: "when your opponent's turn roll is 10, your roll is +2") can read
        # the current roll — the recorded _roll_ctx is not written until the roll-off
        # resolves. Which side's roll the condition reads follows the trigger's `who`,
        # exactly as the OnRoll path does (RollValue docstring).
        ctx = {
            "A": conditions.RollContext(skill=sa, gap=vb - va, value=va, opp_skill=sb),
            "B": conditions.RollContext(skill=sb, gap=va - vb, value=vb, opp_skill=sa),
        }
        for owner in ("A", "B"):
            opp = self.state.opponent_of(owner)
            for eff in self._standing_effects(owner):
                trig = eff.trigger
                if not isinstance(trig, fx.InRoll) or not self._in_roll_matches(
                    trig, owner, rolled
                ):
                    continue
                cond_ctx = ctx[owner if trig.who is fx.Who.SELF else opp]
                if not conditions.holds(eff.condition, self.state, owner, cond_ctx):
                    continue
                for a in eff.actions:
                    if isinstance(a, fx.ModifyRoll) and a.when is fx.RollWhen.THIS:
                        target = owner if a.who is fx.Who.SELF else opp
                        vals[target] += a.delta
        return vals["A"], vals["B"]

    def _in_roll_matches(self, trig: fx.InRoll, owner: str, rolled: dict[str, Skill]) -> bool:
        """Whether an :class:`~srg_sim.effects.InRoll` trigger fires for this roll."""
        if trig.skill is None:
            return True
        if trig.either:  # fires once if EITHER side rolled the skill (capped modifier)
            return trig.skill in rolled.values()
        roller = owner if trig.who is fx.Who.SELF else self.state.opponent_of(owner)
        return rolled[roller] is trig.skill

    @staticmethod
    def _roll_winner(va: int, vb: int, lowest: bool) -> str:
        """The roll-off winner. Highest roll wins, unless a lowest-wins gimmick
        (Fae) flips it to the lowest; A holds the edge on a residual tie."""
        if lowest:
            return "A" if va <= vb else "B"
        return "A" if va >= vb else "B"

    def _lowest_wins(self) -> bool:
        """True iff either side's active gimmick declares the roll-off lowest-wins
        (a Static :class:`~srg_sim.effects.LowestRollWins`; blanked gimmicks drop out
        of ``_standing_effects``, so blanking Fae restores highest-wins)."""
        for key in ("A", "B"):
            for eff in self._standing_effects(key):
                if isinstance(eff.trigger, fx.Static) and any(
                    isinstance(a, fx.LowestRollWins) for a in eff.actions
                ):
                    return True
        return False

    def _record_roll_ctx(self, sa: Skill, va: int, sb: Skill, vb: int) -> None:
        """Stash each side's rolled skill + signed gap (opponent minus self, so a
        positive gap means that side rolled lower) for roll-scoped conditions fired
        this turn (RollGap*/RollWasSkill; DESIGN.md §3)."""
        self._roll_ctx = {
            "A": conditions.RollContext(skill=sa, gap=vb - va, value=va, opp_skill=sb),
            "B": conditions.RollContext(skill=sb, gap=va - vb, value=vb, opp_skill=sa),
        }

    # -- derived stats (with live condition evaluation) --------------------

    def _holds(self, key: str) -> Callable[[fx.Condition], bool]:
        """A condition evaluator bound to ``key`` (resolves conditional buffs/stops)."""
        return lambda cond: conditions.holds(cond, self.state, key)

    def _stats(self, key: str) -> dict[str, int]:
        return self.state.effective_stats(key, self._holds(key))

    def _stat(self, key: str, skill: Skill) -> int:
        return self.state.effective_stat(key, skill, self._holds(key))

    def _roll_for(self, key: str, use_pending: bool) -> tuple[Skill, int]:
        skill = self.state.rng.roll()
        base = self._stat(key, skill)
        mods: list[gl.RollMod] = []
        delta = self.state.players[key].pending_roll_mods["this"] if use_pending else 0
        if delta:
            mods.append(gl.RollMod(src="pending", delta=delta))
        value = base + delta
        self._log(
            gl.Roll(
                t=self.state.turn_no,
                player=key,
                skill=skill.value,
                base=base,
                value=value,
                mods=tuple(mods),
            )
        )
        return skill, value

    def _promote_pending(self) -> None:
        """Fold a queued ``when=NEXT`` roll mod into the imminent roll (#50).

        A ``when=NEXT`` mod (Enjoy Everything, the Bull's comeback, Mastermind's
        ``OnBump`` opp-penalty) is queued into ``next`` during a turn's *action /
        OnRoll* phase — i.e. AFTER that turn's roll-off already ran. Promoting
        ``next -> this`` here, at the START of the following roll-off, makes such a
        mod land on the immediately-following roll, not the turn after (the old
        promote-right-after-the-roll ordering delayed it one full turn)."""
        for player in self.state.players.values():
            mods = player.pending_roll_mods
            mods["this"] += mods["next"]
            mods["next"] = 0

    def _consume_pending(self) -> None:
        """The initial roll spent ``this``; clear it so a pending mod applies once
        (bump re-rolls run with ``use_pending=False``, so they never re-read it)."""
        for player in self.state.players.values():
            player.pending_roll_mods["this"] = 0

    def _tie_winner(self) -> str | None:
        holders = [k for k, p in self.state.players.items() if p.flags.pop("win_tie", False)]
        return holders[0] if len(holders) == 1 else None

    # -- draw / count-out --------------------------------------------------

    def _draw_for_turn(self, key: str) -> bool:
        """Draw for the won turn; return False if the game ended by count-out."""
        player = self.state.players[key]
        if not player.deck and not player.hand:
            self._win(key, "count_out")  # exhausting deck+hand on a won turn is a win
            return False
        self._draw(key, 1)
        return True

    def _draw(self, key: str, n: int, source: fx.DeckEnd = fx.DeckEnd.TOP) -> None:
        player = self.state.players[key]
        if source is fx.DeckEnd.BOTTOM:
            player.deck.reverse()
        drawn = player.draw(n)
        if source is fx.DeckEnd.BOTTOM:
            player.deck.reverse()
        if drawn:
            self._log(
                gl.Draw(
                    t=self.state.turn_no,
                    player=key,
                    cards=[c.db_uuid for c in drawn],
                    source=source.value,
                    hidden=True,  # deck -> hand: both private, opponent sees only the count
                )
            )
            # Hand cap is enforced IMMEDIATELY (DESIGN.md §6): any draw that puts a
            # player over the max forces a discard-down right now — before their play
            # action, on a bump, or after an effect-draw — not batched at end of turn.
            self._hand_cap(key)

    # -- attack sequence ---------------------------------------------------

    def _take_turn_action(self, active: str) -> None:
        """Play ONE card advancing the persistent chain, or pass+bury (DESIGN.md §6).

        Cards resolve into ``in_play`` and stay there across turns; a Finish that
        resolves unstopped triggers the finish sequence.
        """
        defender = self.state.opponent_of(active)
        legal = self._playable_options(active) + [{"kind": "pass"}]
        choice = self._decide("turn_action", active, legal)
        if choice["kind"] == "pass":
            self._do_pass(active)
            return
        card = self._take_from_hand(active, choice["number"])
        if self._resolve_play(active, defender, card) and card.play_order is PlayOrder.FINISH:
            self._finish_sequence(active, defender, card)

    def _do_pass(self, active: str) -> None:
        # Passing recycles one card from discard to the bottom of the deck (§6).
        discard = self.state.players[active].discard
        if not discard:
            return
        legal = [self._card_option(c) for c in discard]
        chosen = self._decide("bury", active, legal)
        card = next(c for c in discard if c.db_uuid == chosen["card"])
        self._bury_cards(active, [card])

    def _bury_cards(self, key: str, cards: list[Card]) -> None:
        """Move ``cards`` from discard to the bottom of the deck (DESIGN.md §5)."""
        player = self.state.players[key]
        for card in cards:
            player.discard.remove(card)
            player.deck.append(card)  # bottom of deck
        self._log(
            gl.Bury(
                t=self.state.turn_no,
                player=key,
                cards=[c.db_uuid for c in cards],
                source="discard",
            )
        )

    # -- play resolution + stops ------------------------------------------

    def _resolve_play(self, active: str, defender: str, card: Card) -> bool:
        # Poison: fold any queued "added text" onto the card BEFORE the stop window,
        # so an "If stopped, ..." injection reaches _apply_stop.
        card = self._apply_pending_text(active, card)
        self._log(
            gl.Play(
                t=self.state.turn_no,
                player=active,
                card=card.db_uuid,
                order=card.play_order.value,
                atk_type=card.atk_type.value,
            )
        )
        # The stop window comes FIRST: a stopped card fires NONE of its text
        # (srg-rules-confirmed; DESIGN.md §6). So OnPlay/OnHit resolve only for an
        # unstopped card. OnPlay fires as the card resolves (before it lands on the
        # board); OnHit after it enters play (for text that reads its own board slot).
        stop = self._offer_stop(defender, active, card)
        if stop is not None:
            self._apply_stop(active, defender, card, stop)
            return False
        # The card's own effects plus any "added text" its owner's active gimmick
        # grants to cards of this name (El Super Santa / Sabu). Injected effects carry
        # their own triggers (OnPlay/OnHit) and dispatch identically. A text-blanked
        # card (opponent's "your Spotlights are blank") fires nothing.
        if self.state.is_text_blanked(card, active):
            effects: list[fx.Effect] = []
        else:
            effects = list(card.effects) + self._injected_text(active, card)
        self._run_effects(effects, fx.OnPlay, active)
        if self._ended():
            return False
        self.state.players[active].in_play.append(card)
        self._run_effects(effects, fx.OnHit, active)  # the card's own "when this hits"
        self._run_hit_gimmicks(card, active)  # owner gimmick "when you hit a <type>" (D1)
        self._enforce_hand_caps()  # a new Static max-handsize mod may force a discard
        return not self._ended()

    def _run_hit_gimmicks(self, card: Card, hitter: str) -> None:
        """Fire the standing ``OnHit`` gimmicks for a card ``hitter`` just hit — gated
        by attack type (D1: "when you hit a Submission, draw 1") and/or the hit card's
        name/text ("when you hit a card with 'X' in the name"). A bare OnHit (no gate)
        is the card's own "when this hits" (already resolved via :meth:`_run_effects`)
        — skipped UNLESS it sets ``on_any`` ("when you hit a card" — Bartholomew Hooke),
        which fires on every hit. ``on_any`` is override-only, so parser fragments that
        produce a bare OnHit stay inert."""
        # The hit card is already on the board here, so a "for each OTHER … in play"
        # count must drop it (Draw.per_excludes_trigger).
        self._hit_card = card
        try:
            for key in ("A", "B"):
                self._run_hit_gimmicks_for(card, key, hitter)
        finally:
            self._hit_card = None

    def _run_hit_gimmicks_for(self, card: Card, key: str, hitter: str) -> None:
        """``key``'s OnHit gimmicks for a hit made by ``hitter``. Both players are
        scanned so an ``OnHit(who=OPP)`` gimmick fires for the NON-hitter."""
        for eff in self._standing_effects(key):
            trig = eff.trigger
            if not isinstance(trig, fx.OnHit):
                continue
            # Whose hit this fires on. The default (SELF) reproduces the pre-v43
            # behavior exactly: only the hitter's own gimmicks fire.
            subject = key if trig.who is fx.Who.SELF else self.state.opponent_of(key)
            if subject != hitter:
                continue
            has_name_gate = bool(trig.name_contains or trig.text_contains)
            if (
                trig.atk_type is None
                and not has_name_gate
                and trig.order is None
                and not trig.on_any
            ):
                continue
            type_ok = trig.atk_type is None or trig.atk_type is card.atk_type
            # "When you hit a Lead" — the play-order gate on the HIT card (ANDed).
            order_ok = trig.order is None or trig.order is card.play_order
            gate = fx.CardFilter(
                name_contains=trig.name_contains, text_contains=trig.text_contains
            )
            if type_ok and order_ok and conditions.card_matches(card, gate):
                self._fire_if_ready(eff, key, None)

    def _injected_text(self, key: str, card: Card) -> list[fx.Effect]:
        """"Added text" effects ``key``'s active gimmicks grant to ``card`` (El Super
        Santa: cards with "Super" in the name gain "Draw 2"). Collects ``AddText``
        actions from ``key``'s standing Static effects whose condition holds and whose
        ``name_contains`` matches the card's title, returning the effects to run
        alongside the card's own. Empty when no gimmick text applies."""
        out: list[fx.Effect] = []
        for eff in self._standing_effects(key):
            if not isinstance(eff.trigger, fx.Static) or not conditions.holds(
                eff.condition, self.state, key
            ):
                continue
            for action in eff.actions:
                if isinstance(action, fx.AddText):
                    gate = fx.CardFilter(name_contains=action.name_contains)
                    if conditions.card_matches(card, gate):
                        out.extend(action.effects)
        return out

    def _offer_stop(self, defender: str, attacker: str, card: Card) -> Card | None:
        stops = self._legal_stops(defender, attacker, card)
        if not stops:
            return None
        # The "none" option carries what is being defended, so a policy can reserve
        # stops for the real threat (a Finish) rather than spend them on cheap bait.
        none: Option = {
            "kind": "none",
            "vs_order": card.play_order.value,
            "vs_type": card.atk_type.value,
        }
        legal = [none] + [self._stop_option(c) for c in stops]
        choice = self._decide("stop", defender, legal)
        if choice["kind"] == "none":
            return None
        return self._take_from_hand(defender, choice["number"])

    @staticmethod
    def _stop_option(card: Card) -> Option:
        return {
            "kind": "stop",
            "number": card.number,
            "card": card.db_uuid,
            "order": card.play_order.value,
            "atk_type": card.atk_type.value,
        }

    def _legal_stops(self, defender: str, attacker: str, card: Card) -> list[Card]:
        return [
            c for c in self.state.players[defender].hand if self._card_can_stop(defender, c, card)
        ]

    def _card_can_stop(self, defender: str, stopper: Card, attack: Card) -> bool:
        """Text-driven stop (DESIGN.md §6): a card can stop ``attack`` iff one of its
        parsed ``Stop`` effects matches the attack's order/type and that effect's
        condition holds from the defender's view (skill stops, see-1, crowd-meter
        gates all fall out of the condition). Cards with no Stop effect cannot stop;
        an attack that declares itself ``Unstoppable`` by the stopper's play order
        cannot be stopped by it.
        """
        if _is_unstoppable_by(attack, stopper):
            return False
        if self.state.is_text_blanked(stopper, defender):
            return False  # a text-blanked stop card cannot stop
        if self._stop_suppressed(defender, stopper):
            return False  # Jokerfish "your cards #N-N cannot stop cards"
        return any(
            conditions.holds(eff.condition, self.state, defender)
            and _attacker_meets_tag_gates(eff, attack)
            and any(
                isinstance(action, fx.Stop) and self._stop_matches_for(defender, action, attack)
                for action in eff.actions
            )
            for eff in stopper.effects
        )

    def _stop_suppressed(self, defender: str, stopper: Card) -> bool:
        """Whether ``defender`` declares that ``stopper`` (by its deck number) cannot
        act as a Stop — Jokerfish V2's ``SuppressStop`` number range."""
        n = stopper.number
        return self._declares_static(
            defender, fx.SuppressStop, lambda a: a.number_min <= n <= a.number_max
        )

    def _stop_matches_for(self, defender: str, stop: fx.Stop, attack: Card) -> bool:
        """Whether ``stop``'s order/type filter covers ``attack``, honoring ``defender``'s
        active ``StopCountsOrderAs`` reframes: an attack whose order is reframed also
        satisfies a ``Stop`` of the reframed order ("your opponent's Finishes are also
        Follow Ups for your Stop cards"). ``None`` order = any."""
        if stop.order is not None and stop.order is not attack.play_order:
            reframed = self._declares_static(
                defender,
                fx.StopCountsOrderAs,
                lambda a: a.attack_order is attack.play_order and a.as_order is stop.order,
            )
            if not reframed:
                return False
        return stop.atk_type is None or stop.atk_type is attack.atk_type

    def _apply_stop(self, active: str, defender: str, attack: Card, stop: Card) -> None:
        # Only the stopped ATTACK goes to the attacker's discard; the stopping card
        # is played onto the defender's board and persists (DESIGN.md §6). A Follow Up
        # used as a stop enters play even with no Lead — stopping bypasses the
        # play-sequence gate. Stops thus build board state (combo/finish bonuses,
        # see-1 enablers) and clear only on a breakout.
        self.state.players[active].discard.append(attack)
        self.state.players[defender].in_play.append(stop)
        self._log(
            gl.Stop(
                t=self.state.turn_no,
                player=defender,
                card=stop.db_uuid,
                stopped=attack.db_uuid,
                reason=f"{stop.atk_type.value} stops {attack.atk_type.value}",
            )
        )
        self._run_effects(stop.effects, fx.OnHit, defender)
        self._run_hit_gimmicks(stop, defender)  # a stop entering play is itself a hit
        # "The stopped card has blank text until the end of the turn" must resolve
        # BEFORE the stopped card's own OnStop: the whole point of that family is to
        # suppress the stopped card's "If Stopped" text, several members reading "stop
        # any card WITH 'If Stopped' in the text: that card has blank text ...". Split
        # so those effects land first and the rest keep their original order.
        self._stopped_card = attack.db_uuid
        blanking = [
            e for e in stop.effects if any(isinstance(a, fx.BlankStoppedText) for a in e.actions)
        ]
        rest = [
            e
            for e in stop.effects
            if not any(isinstance(a, fx.BlankStoppedText) for a in e.actions)
        ]
        self._run_effects(blanking, fx.OnStop, defender)
        # A blanked card fires nothing — the same rule _play_card / _card_can_stop
        # already apply to a text-blanked card.
        if not self.state.is_text_blanked(attack, active):
            self._run_effects(attack.effects, fx.OnStop, active)  # "if this is stopped"
        self._run_effects(rest, fx.OnStop, defender)  # stop card: "when this stops"
        self._stopped_card = None
        # Standing competitor/entrance OnStop, dir-aware from each owner's POV: the
        # attacker's card was stopped (YOURS), the defender stopped a card (THEIRS =
        # "when you Stop a card", e.g. Gia).
        self._run_on_stop_gimmicks(active, fx.Direction.YOURS, attack.play_order)
        self._run_on_stop_gimmicks(defender, fx.Direction.THEIRS, attack.play_order)

    def _run_on_stop_gimmicks(
        self, key: str, direction: fx.Direction, stopped: PlayOrder
    ) -> None:
        """Fire ``key``'s standing (gimmick/entrance) ``OnStop`` effects whose ``dir``
        matches — THEIRS for the stopper ("when you Stop a card"), YOURS for the
        stopped attacker — and whose optional ``order`` gate matches the STOPPED
        card's play order (``None`` = any). Unlike :meth:`_run_effects` (trigger-type
        match only), this consults both ``OnStop.dir`` and ``OnStop.order``."""
        for eff in self._gimmick_standing_effects(key):
            trig = eff.trigger
            if (
                isinstance(trig, fx.OnStop)
                and trig.dir is direction
                and (trig.order is None or trig.order is stopped)
            ):
                self._fire_if_ready(eff, key, None)

    def _run_on_bury(self, buried_by: str, from_hand: bool, is_discard: bool) -> None:
        """Fire standing ``OnBury`` gimmicks after an EFFECT-caused bury/discard landed
        on ``buried_by`` (The Cyclone V1, Tommy Stillwell). ``from_hand`` = the cards
        left the hand (vs the discard pile); ``is_discard`` = the event was a discard
        (vs a bury). Scans BOTH players so a ``who=OPP`` variant works; fires once per
        event. The mechanical pass-and-recycle bury and the hand-cap trim bypass
        ``_act_bury``/``_act_discard``, so they never reach here (DESIGN.md §3)."""
        for owner in (buried_by, self.state.opponent_of(buried_by)):
            for eff in self._standing_effects(owner):
                trig = eff.trigger
                if not isinstance(trig, fx.OnBury):
                    continue
                # SELF fires when the effect's owner is the burier; OPP when the burier
                # is the owner's opponent.
                if (trig.who is fx.Who.SELF) != (owner == buried_by):
                    continue
                if is_discard and not trig.also_discard:
                    continue  # a discard only fires the "bury or discard" variant
                if trig.from_hand_only and not from_hand:
                    continue  # hand-only variant ignores discard-pile buries
                self._fire_if_ready(eff, owner, None)

    # -- finish sequence + breakout ---------------------------------------

    def _finish_sequence(self, finisher: str, defender: str, card: Card) -> None:
        skill = self.state.rng.roll()
        # Switch-rolled-skill also applies to the Finish roll (Scott Prime): switch
        # before base/combo are computed so they recompute from the new skill.
        to = self._find_switch(finisher, skill)
        if to is not None:
            self._log_effect(finisher, "SwitchRolledSkill", finisher, {"from": skill.value, "to": to.value, "roll": "finish"})
            skill = to
        base = self._stat(finisher, skill)
        # The whole in-play combo pays off: sum every card's printed bonus for the
        # rolled skill, plus any flat "+N to your Finish rolls" (DESIGN.md §5). A card
        # that reads "if you bumped, double these bonuses" (T-Virus) doubles its own
        # contribution when this turn's roll-off involved a bump.
        bonus = sum(self._card_finish_bonus(c, skill) for c in self.state.players[finisher].in_play)
        bonus += self._finish_roll_bonus(finisher, skill)
        cm = self.state.crowd_meter
        value = base + bonus + cm
        auto = is_auto_success(value, cm)
        self._log_finish_attempt(finisher, card, skill, bonus, value, cm, auto)
        # "When you roll <skill> for your Finish roll" gimmicks fire here, after the
        # roll is determined but before it resolves (The Man from I.T.). No deck card
        # carries OnFinishRoll, so the frozen finish games are untouched.
        self._run_on_finish_roll(finisher, skill, value)
        if self._ended():
            return
        if not auto and self._breakout(defender, value):
            self._on_broken_out(finisher)  # defender broke out; the match resumes
            return
        self._win(finisher, "finish")

    def _card_finish_bonus(self, card: Card, skill: Skill) -> int:
        """A single in-play card's Finish-roll combo bonus for ``skill``, doubled when
        the card declares ``DoubleFinishIfBumped`` and this turn's roll-off bumped."""
        bonus = card.bonus_for(skill)
        if self._turn_bumped and any(
            isinstance(a, fx.DoubleFinishIfBumped) for eff in card.effects for a in eff.actions
        ):
            bonus *= 2
        return bonus

    def _finish_roll_bonus(self, key: str, skill: Skill) -> int:
        """ "+N to your Finish rolls" from the finisher's live effects (in-play combo,
        gimmick, entrance), each gated by its condition and by its ``when_skill`` (a
        skill-specific bonus applies only when that skill is rolled; DESIGN.md §5)."""
        total = 0
        for eff in self._standing_effects(key):
            if not conditions.holds(eff.condition, self.state, key):
                continue
            for a in eff.actions:
                if isinstance(a, fx.FinishRollBonus) and (
                    a.when_skill is None or a.when_skill is skill
                ):
                    # Flat delta, or delta * (count of per_who's cards in per_zone
                    # matching the filter) — "+1 per Spotlight in play".
                    if a.per is None:
                        total += a.delta
                    else:
                        who = key if a.per_who is fx.Who.SELF else self.state.opponent_of(key)
                        total += a.delta * self.state._count_in_zone(a.per, a.per_zone, who)
        return total

    def _log_finish_attempt(
        self, finisher: str, card: Card, skill: Skill, bonus: int, value: int, cm: int, auto: bool
    ) -> None:
        self._log(
            gl.FinishAttempt(
                t=self.state.turn_no,
                player=finisher,
                finish=card.db_uuid,
                value=value,
                crowd_meter=cm,
                auto_success=auto,
                bonus={skill.value: bonus} if bonus else {},
            )
        )

    def _breakout_bonus(self, defender: str, attempt_no: int) -> int:
        """Total breakout-roll modifier for ``defender``'s attempt number
        ``attempt_no`` (1-indexed): the sum of active ``BreakoutModifier`` deltas
        from the defender's own standing effects (gimmick/entrance/in-play combo),
        each gated by its condition. An ``attempts`` gate restricts a modifier to a
        single attempt ("your 3rd breakout roll each turn is +2"); ``None`` applies
        to every attempt. Scans the same standing set as ``_finish_roll_bonus``."""
        total = 0
        for eff in self._standing_effects(defender):
            if not conditions.holds(eff.condition, self.state, defender):
                continue
            for a in eff.actions:
                if isinstance(a, fx.BreakoutModifier) and (
                    a.attempts is None or a.attempts == attempt_no
                ):
                    total += a.delta
        return total

    def _breakout(self, defender: str, finish_value: int) -> bool:
        cm = self.state.crowd_meter
        rolls: list[gl.BreakoutRoll] = []
        broke = False
        for i in range(BREAKOUT_ATTEMPTS):
            skill = self.state.rng.roll()
            val = self._stat(defender, skill)
            # A BreakoutModifier{delta} raises the roll by delta; passing it as a
            # NEGATIVE penalty keeps the raw-10-always-breaks rule on the unboosted
            # value (a boosted 8->10 is not a "raw 10"). No modifier -> penalty 0 ->
            # byte-identical to before (the frozen corpus has none).
            penalty = -self._breakout_bonus(defender, i + 1)
            success = stat_breaks_out(val, finish_value, penalty, cm)
            rolls.append(
                gl.BreakoutRoll(skill=skill.value, value=val, penalty=penalty, success=success)
            )
            if success:
                broke = True
                break
        self._log(
            gl.Breakout(
                t=self.state.turn_no, defender=defender, broke_out=broke, rolls=tuple(rolls)
            )
        )
        return broke

    def _on_broken_out(self, finisher: str) -> None:
        # OnBreakout fires FIRST, while sources are still in play — a card-based recur
        # ("if your opponent breaks out, shuffle Spotlights…") needs its card present
        # before the boards clear. `who` selects whose breakout fires it (None = any);
        # the defender is the breaker. A no-op for decks without OnBreakout, so the
        # frozen corpus (which has none) is byte-identical.
        breaker = self.state.opponent_of(finisher)
        for key in ("A", "B"):
            for eff in self._standing_effects(key):
                if not isinstance(eff.trigger, fx.OnBreakout):
                    continue
                who = eff.trigger.who
                target = key if who is fx.Who.SELF else self.state.opponent_of(key)
                if who is None or target == breaker:
                    self._fire_if_ready(eff, key, None)
        # Breakout: ALL cards in play on BOTH sides clear to discard (§5), CM +1.
        for key in ("A", "B"):
            self._discard_in_play(key)
        self.state.crowd_meter += 1
        self._log(gl.CrowdMeter(t=self.state.turn_no, delta=1, value=self.state.crowd_meter))

    # -- end of turn -------------------------------------------------------

    def _hand_cap(self, key: str) -> None:
        # The cap is continuous (DESIGN.md §6): whenever a player sits above their
        # maximum hand size — after a draw, or after an opponent's card lowers it —
        # they discard down right now. The max is derived (base + Static MaxHandSize
        # mods). Over it, the owner chooses which to shed (DESIGN.md §6/§7).
        cap = self.state.effective_hand_cap(key, HAND_CAP, self._holds(key))
        excess = len(self.state.players[key].hand) - cap
        if excess > 0:
            self._discard_from_hand(key, excess, random=False)

    def _enforce_hand_caps(self) -> None:
        # A card entering play can lower the *opponent's* max hand size, forcing them
        # to discard down with no draw of their own — so re-check both sides whenever
        # the board changes (DESIGN.md §6).
        for key in self.state.players:
            self._hand_cap(key)

    def _discard_in_play(self, key: str) -> None:
        player = self.state.players[key]
        if not player.in_play:
            return
        cards = list(player.in_play)
        player.in_play.clear()
        player.discard.extend(cards)
        self._log(gl.Discard(t=self.state.turn_no, player=key, cards=[c.db_uuid for c in cards]))

    # -- effect executor ---------------------------------------------------

    def _standing_effects(self, key: str) -> tuple[fx.Effect, ...]:
        """All effects currently able to fire for ``key``: gimmick (unless blanked),
        entrance, and in-play cards."""
        out = list(self._gimmick_standing_effects(key))
        for card in self.state.players[key].in_play:
            out.extend(card.effects)
        return tuple(out)

    def _gimmick_standing_effects(self, key: str) -> tuple[fx.Effect, ...]:
        """The persistent standing effects that are *not* a played card: competitor
        gimmick (unless blanked, flip-aware) + entrance. Fired for standing ``OnStop``
        gimmicks in a stop exchange, where re-scanning in-play cards would re-fire the
        stop card that just entered play (:meth:`_apply_stop`)."""
        player = self.state.players[key]
        out: list[fx.Effect] = []
        if not self.state.is_gimmick_blanked(key):
            gimmick = player.competitor.effects
            if self._gimmick_signs_flipped(key):  # Cassandra flips this player's gimmick
                gimmick = tuple(fx.flip_signs(e) for e in gimmick)
            out.extend(gimmick)
        out.extend(player.entrance.effects)
        return tuple(out)

    def _gimmick_signs_flipped(self, key: str) -> bool:
        """True iff ``key``'s opponent has an active (unblanked) ``Static``
        :class:`~srg_sim.effects.FlipGimmickSigns` — Cassandra negating every printed
        +/- on ``key``'s gimmick. Reads the opponent's raw competitor effects (not their
        standing set) so it never recurses back through :meth:`_standing_effects`."""
        opp = self.state.opponent_of(key)
        if self.state.is_gimmick_blanked(opp):
            return False
        return any(
            isinstance(eff.trigger, fx.Static)
            and any(isinstance(a, fx.FlipGimmickSigns) for a in eff.actions)
            for eff in self.state.players[opp].competitor.effects
        )

    def _run_effects(
        self,
        effects: tuple[fx.Effect, ...],
        trigger: type[fx.IRNode],
        key: str,
        roll: conditions.RollContext | None = None,
    ) -> None:
        """Fire every effect whose trigger matches, condition holds, and frequency
        guard permits (DESIGN.md §3). ``roll`` supplies the roll context so
        ``RollGap*`` / ``RollWasSkill`` conditions resolve on turn-roll triggers;
        it is ``None`` (those conditions then fail) at non-roll trigger points."""
        for eff in effects:
            if isinstance(eff.trigger, trigger):
                self._fire_if_ready(eff, key, roll)

    def _fire_if_ready(self, eff: fx.Effect, key: str, roll: conditions.RollContext | None) -> None:
        """Fire one effect if its frequency guard permits and its condition holds
        (the trigger is matched by the caller). Shared by trigger dispatch and the
        skill/who-matched OnRoll path so both honour condition + frequency alike."""
        if not (
            self._may_fire(eff, key) and conditions.holds(eff.condition, self.state, key, roll)
        ):
            return
        if eff.optional and not self._take_optional(eff, key):
            return  # declined "you may" — leaves the freq guard unspent
        self._mark_fired(eff, key)
        self._apply_actions(eff, key)

    def _take_optional(self, eff: fx.Effect, key: str) -> bool:
        """Offer a "you may" effect to its owner (DESIGN.md §3 ``Effect.optional``).
        The card controller decides — a close approximation for the rare rider whose
        text lets the *opponent* decide (e.g. Big Body Block), noted in its clause."""
        legal: list[Option] = [
            {"kind": "yes", "clause": eff.raw_clause},
            {"kind": "no", "clause": eff.raw_clause},
        ]
        return self._decide("optional", key, legal)["kind"] == "yes"

    def _apply_actions(self, eff: fx.Effect, key: str) -> None:
        # The granting clause, read only by a TIMED BuffSkill for its stacking
        # identity. `_act_choice` resolves inside this call, so it inherits it.
        self._clause = eff.raw_clause
        for action in eff.actions:
            self._apply_action(action, key)
            if self._resolve_pending():
                return

    def _apply_action(self, action: fx.ActionOrUnsupported, key: str) -> None:
        handler = _ACTIONS.get(type(action))
        if handler is None:
            self._log_unsupported(key, repr(action), f"action {type(action).__name__} not modeled")
            return
        handler(self, action, key)

    # individual action handlers (kept tiny for the complexity gate) --------

    def _per_multiplier(
        self, per: fx.CardFilter, per_who: fx.Who, key: str, exclude: Card | None = None
    ) -> int:
        """Count of ``per``-matching cards on ``per_who``'s board (honoring
        ``CountsAsInPlay``), the scale for a per-count Draw/Discard/ModifyRoll. A
        "for each other … in play" clause is normally authored ``OnPlay`` so the source
        card is not yet on the board and needs no self-exclusion; ``exclude`` covers
        the ``OnHit`` case, where the hit card IS already in play."""
        counter = key if per_who is fx.Who.SELF else self.state.opponent_of(key)
        return conditions.count_in_play(self.state.players[counter].in_play, per, exclude)

    def _act_draw(self, action: fx.Draw, key: str) -> None:
        target = key if action.who is fx.Who.SELF else self.state.opponent_of(key)
        n = action.n
        if action.per is not None:
            exclude = self._hit_card if action.per_excludes_trigger else None
            n *= self._per_multiplier(action.per, action.per_who, key, exclude)
            if action.cap is not None:  # "(Max 3)" clamps the per-count product
                n = min(n, action.cap)
        if n:
            # "Your opponent does not draw for your card effects" (Sami "The Draw"):
            # a draw this player's effect grants the opponent is voided.
            if action.who is fx.Who.OPP and self._suppresses_opp_draw(key):
                self._log_effect(key, "SuppressOpponentDraw", target, {"n": n})
            else:
                self._draw(target, n, action.source)

    def _suppresses_opp_draw(self, key: str) -> bool:
        """Whether ``key`` holds an active "your opponent does not draw for your card
        effects" declaration (Sami "The Draw"). Read at ``_act_draw``."""
        return self._declares_static(key, fx.SuppressOpponentDraw)

    def _suppresses_self_hand_loss(self, key: str, target: str) -> bool:
        """Whether ``key``'s OWN effect must not cost ``target`` cards from hand — Sami
        "Death Machine" V2: "you do not bury or discard cards from your hand for your
        own card effects". Scoped to self-inflicted loss (``key == target``), so an
        opponent's effect still takes the cards."""
        return key == target and self._declares_static(key, fx.SuppressSelfHandLoss)

    def _declares_static(
        self,
        key: str,
        node: type[fx.IRNode],
        pred: Callable[[Any], bool] | None = None,
    ) -> bool:
        """Whether ``key`` holds an active Static declaration of a ``node`` action — on
        their own gimmick (unless blanked), entrance, or in-play, with the declaration's
        own condition holding. Optional ``pred`` further filters on the action's fields
        (e.g. a deck-number range). The read side of the passive-flag actions, never
        executed."""
        for effects, active in self.state._buff_sources(key, self.state.players[key]):
            if not active:
                continue
            for eff in effects:
                if not isinstance(eff.trigger, fx.Static):
                    continue
                if not any(isinstance(a, node) and (pred is None or pred(a)) for a in eff.actions):
                    continue
                if conditions.holds(eff.condition, self.state, key):
                    return True
        return False

    def _act_shuffle_deck(self, action: fx.ShuffleDeck, key: str) -> None:
        target = key if action.who is fx.Who.SELF else self.state.opponent_of(key)
        self._log_effect(key, "ShuffleDeck", target, None)
        self._shuffle_deck(target)

    def _shuffle_deck(self, key: str) -> None:
        """Shuffle ``key``'s deck as an EFFECT-caused shuffle and fire any OnShuffle
        gimmicks. The match-start setup shuffle and the private bury-ordering shuffle
        deliberately bypass this (not a card/gimmick "shuffling your deck")."""
        self.state.rng.shuffle(self.state.players[key].deck)
        self._run_on_shuffle(key)

    def _run_on_shuffle(self, shuffled: str) -> None:
        """Fire standing OnShuffle gimmicks after ``shuffled``'s deck was shuffled by an
        effect. Scans BOTH players so a ``who=OPP`` ("when your opponent shuffles their
        deck" — Memes Dealer V2) variant works; fires once per shuffle."""
        for owner in (shuffled, self.state.opponent_of(shuffled)):
            for eff in self._standing_effects(owner):
                trig = eff.trigger
                if not isinstance(trig, fx.OnShuffle):
                    continue
                # SELF fires when the owner shuffled their own deck; OPP when the
                # shuffled deck belongs to the owner's opponent.
                if (trig.who is fx.Who.SELF) == (owner == shuffled):
                    self._fire_if_ready(eff, owner, None)

    def _run_on_discard_move(self, pile: str) -> None:
        """Fire standing OnDiscardMove gimmicks after an effect moved one or more cards
        OUT of ``pile``'s discard pile. Scans BOTH players so a ``who=OPP`` variant
        ("when your opponent moves any number of cards from their discard pile" —
        Brumeister V2) works; fires once per action, however many cards moved."""
        for owner in (pile, self.state.opponent_of(pile)):
            for eff in self._standing_effects(owner):
                trig = eff.trigger
                if not isinstance(trig, fx.OnDiscardMove):
                    continue
                # SELF fires when the owner's own pile was drawn from; OPP when the
                # pile belongs to the owner's opponent.
                if (trig.who is fx.Who.SELF) == (owner == pile):
                    self._fire_if_ready(eff, owner, None)

    def _bury_from_discard(
        self, selector: fx.CardFilter, count: int, who: fx.Who, random: bool, choose: bool, key: str
    ) -> None:
        """Bury ``count`` card(s) from a discard pile to their owner's deck bottom.

        A discard pile has **no meaningful order**, so the bury is a CHOICE: the actor
        picks any card in the pile (``random`` picks at random instead). ``choose``
        widens the pool to BOTH piles — "bury 1 card in any player's discard pile"
        (Cherry Glamazon); otherwise it is ``who``'s pile. ``selector`` filters the
        candidates. Fires OnDiscardMove for the pile that lost a card."""
        piles = (
            [key, self.state.opponent_of(key)]
            if choose
            else [key if who is fx.Who.SELF else self.state.opponent_of(key)]
        )
        for _ in range(count):
            legal: list[Option] = []
            for pile in piles:
                for c in self.state.players[pile].discard:
                    if conditions.card_matches(c, selector):
                        opt = dict(self._discard_option(c))
                        opt["owner"] = pile
                        legal.append(opt)
            if not legal:
                return
            chosen = self.state.rng.reveal(legal) if random else self._decide("bury", key, legal)
            owner = str(chosen["owner"])
            uuid = str(chosen["card"])
            pile_cards = self.state.players[owner].discard
            card = next((c for c in pile_cards if c.db_uuid == uuid), None)
            if card is None:
                return
            self._bury_cards(owner, [card])  # performs the discard -> deck-bottom move
            self._run_on_bury(owner, from_hand=False, is_discard=False)
            self._run_on_discard_move(owner)

    def _remove_from_either_board(
        self, selector: fx.CardFilter, count: int, key: str
    ) -> None:
        """"Choose 1 card in play and discard it" with no side restriction (Cherry
        Glamazon): the actor picks from EITHER board and the card goes to its OWNER's
        discard. Mirrors _act_return_to_hand's `choose` branch."""
        boards = [key, self.state.opponent_of(key)]
        for _ in range(count):
            legal: list[Option] = []
            for b in boards:
                for c in self.state.players[b].in_play:
                    if conditions.card_matches(c, selector):
                        opt = dict(self._card_option(c))
                        opt["owner"] = b
                        legal.append(opt)
            if not legal:
                return
            chosen = self._decide("target", key, legal)
            owner = str(chosen["owner"])
            uuid = str(chosen["card"])
            board = self.state.players[owner].in_play
            card = next((c for c in board if c.db_uuid == uuid), None)
            if card is None:
                return
            board.remove(card)
            self.state.players[owner].discard.append(card)
            self._log(
                gl.Discard(
                    t=self.state.turn_no, player=owner, cards=[card.db_uuid], source="in_play"
                )
            )

    def _act_bury(self, action: fx.Bury, key: str) -> None:
        if action.source is fx.BuryFrom.DISCARD:
            self._bury_from_discard(
                action.selector, action.count, action.who, action.random, action.choose, key
            )
            return
        target = key if action.who is fx.Who.SELF else self.state.opponent_of(key)
        if self._suppresses_self_hand_loss(key, target):
            self._log_effect(key, "SuppressSelfHandLoss", target, {"n": action.count})
            return
        # ``choose`` makes the EFFECT OWNER pick which of the target's hand cards to
        # bury (The Man from I.T. looks at the opponent's hand and chooses); otherwise
        # the hand owner sheds their least valuable.
        chooser = key if action.choose else target
        n = self._bury_from_hand(target, chooser, action.count, action.random, action.selector)
        if n > 0:
            self._run_on_bury(target, from_hand=True, is_discard=False)  # effect-caused hand bury

    def _bury_from_hand(
        self, key: str, chooser: str, count: int, random: bool, selector: fx.CardFilter
    ) -> int:
        """"Bury N cards in [your/their] hand": move ``count`` cards from ``key``'s
        hand to the bottom of their deck. The hand owner chooses which (their hidden
        hand) unless ``random``; when ``chooser`` is the effect owner (not the hand
        owner) it is the attacker picking from the opponent's hand, a distinct decision
        point that disrupts the most valuable card. Mirrors :meth:`_discard_from_hand`
        but lands the cards on the deck bottom and logs a ``Bury`` from ``hand``."""
        player = self.state.players[key]
        point = "bury_hand" if chooser == key else "bury_opp_hand"
        buried: list[Card] = []
        for _ in range(count):
            pool = [c for c in player.hand if conditions.card_matches(c, selector)]
            if not pool:
                break
            card = self.state.rng.reveal(pool) if random else self._pick_from(chooser, pool, point)
            player.hand.remove(card)
            buried.append(card)
        if buried:
            for card in buried:
                player.deck.append(card)  # bottom of deck
            self._log(
                gl.Bury(
                    t=self.state.turn_no,
                    player=key,
                    cards=[c.db_uuid for c in buried],
                    source="hand",
                )
            )
        return len(buried)

    def _pick_from(self, key: str, cards: list[Card], point: str) -> Card:
        """Let ``key``'s policy pick one of ``cards`` for a recur/tutor selection —
        the card's owner chooses which to recover/search (DESIGN.md §7), not the
        engine. Auto-taken (and unlogged) when only one card matches."""
        if len(cards) == 1:
            return cards[0]
        legal = [self._discard_option(c) for c in cards]
        chosen = self._decide(point, key, legal)
        return next(c for c in cards if c.db_uuid == chosen["card"])

    def _pick_optional_from(self, key: str, cards: list[Card], point: str) -> Card | None:
        """Like :meth:`_pick_from` but for an "up to" selection: the owner may stop
        early (a trailing ``none`` option). ``None`` = decline. The default policy
        takes the first card (``legal[0]``), so the ``none`` option comes last."""
        legal = [self._discard_option(c) for c in cards] + [{"kind": "none"}]
        chosen = self._decide(point, key, legal)
        if chosen["kind"] == "none":
            return None
        return next(c for c in cards if c.db_uuid == chosen["card"])

    def _act_search(self, action: fx.Search, key: str) -> None:
        # Tutor: the searcher picks a matching deck card, then shuffles the deck (you
        # looked through it). dest=HAND puts it in hand; dest=DECK_TOP puts it back on
        # TOP of the shuffled deck (Heartache Kid); dest=DISCARD is a separate
        # mill-to-discard line (Destiny's Call V2; DESIGN.md §3, #49).
        if action.dest is fx.Dest.DISCARD:
            self._search_to_discard(action, key)
            return
        player = self.state.players[key]
        matches = [c for c in player.deck if conditions.card_matches(c, action.filter)]
        card = self._pick_from(key, matches, "target") if matches else None
        if card is not None:
            player.deck.remove(card)
        # The picked card is out of the deck for the shuffle either way, so a HAND
        # search shuffles identically to before (byte-for-byte parity).
        self._shuffle_deck(key)
        if card is not None:
            if action.dest is fx.Dest.DECK_TOP:
                player.deck.insert(0, card)  # top of deck
            else:
                player.hand.append(card)
            self._log(
                gl.Search(
                    t=self.state.turn_no,
                    player=key,
                    cards=[card.db_uuid],
                    source="deck",
                    hidden=True,  # deck -> hand/deck: both private, opponent sees only counts
                )
            )
        if action.dest is fx.Dest.HAND:
            self._hand_cap(key)

    def _search_to_discard(self, action: fx.Search, key: str) -> None:
        # "Search your deck for up to N cards and put them into your discard pile."
        # The owner looks through the deck and chooses which (and how many, up to N)
        # to bin — a setup line for discard-fuelled recursion (DESIGN.md §7). The
        # binned cards are face-up in the (public) discard, so the move is logged
        # openly. Searching disturbs the deck, so it shuffles afterwards.
        player = self.state.players[key]
        for _ in range(action.count):
            matches = [c for c in player.deck if conditions.card_matches(c, action.filter)]
            if not matches:
                break
            card = self._pick_optional_from(key, matches, "search")
            if card is None:
                break  # "up to" — the owner may stop early
            player.deck.remove(card)
            player.discard.append(card)
            self._log(
                gl.Discard(
                    t=self.state.turn_no,
                    player=key,
                    cards=[card.db_uuid],
                    source="deck",
                    hidden=False,  # deck -> discard: the binned card is public in discard
                )
            )
        self._shuffle_deck(key)

    def _act_shuffle_into_deck(self, action: fx.ShuffleIntoDeck, key: str) -> None:
        # Recur ONE matching card from discard into the deck, then shuffle. The IR
        # node has no count, so "shuffle 2 / up to 3 cards" is authored as repeated
        # ShuffleIntoDeck actions (no IR change; DESIGN.md §3 review gate).
        player = self.state.players[key]
        matches = [c for c in player.discard if conditions.card_matches(c, action.selector)]
        if matches:
            card = self._pick_from(key, matches, "target")
            player.discard.remove(card)
            player.deck.append(card)
            self._log(
                gl.Bury(  # discard -> deck movement (the shuffle rides the RNG state)
                    t=self.state.turn_no, player=key, cards=[card.db_uuid], source="discard"
                )
            )
            # The card has left the pile; fires ahead of the shuffle's own OnShuffle.
            self._run_on_discard_move(key)
        self._shuffle_deck(key)

    def _act_add_from_discard(self, action: fx.AddFromDiscard, key: str) -> None:
        # Recur a matching card from discard to hand ("add 1 <type> from your
        # discard pile to your hand"); the owner chooses which (DESIGN.md §7).
        player = self.state.players[key]
        matches = [c for c in player.discard if conditions.card_matches(c, action.filter)]
        if not matches:
            return
        card = self._pick_from(key, matches, "target")
        player.discard.remove(card)
        player.hand.append(card)
        self._log(
            gl.Search(  # discard (public) -> hand: which card left discard is visible
                t=self.state.turn_no, player=key, cards=[card.db_uuid], source="discard"
            )
        )
        self._run_on_discard_move(key)
        self._hand_cap(key)

    def _act_swap_hand_discard(self, action: fx.SwapHandDiscard, key: str) -> None:
        # "Switch 1 card in your hand with 1 card in your discard pile" (Collin, Mr.
        # Rey): pick one hand card out (-> discard, via the `discard`/shed point) and
        # one discard card in (-> hand, via the `target`/tutor point). No-op if either
        # zone is empty; a 1-for-1 swap so both sizes are preserved.
        player = self.state.players[key]
        if not player.hand or not player.discard:
            return
        out = self._pick_from(key, list(player.hand), "discard")  # hand card leaving
        into = self._pick_from(key, list(player.discard), "target")  # discard card entering
        player.hand.remove(out)
        player.discard.remove(into)
        player.hand.append(into)
        player.discard.append(out)
        self._log_effect(key, "SwapHandDiscard", key, {"hand_out": out.db_uuid, "discard_in": into.db_uuid})
        self._run_on_discard_move(key)

    def _act_recur_to_deck_top(self, action: fx.RecurToDeckTop, key: str) -> None:
        # Put up to `count` matching cards from discard ON TOP of the deck ("Put up
        # to 3 Finishes from your discard pile on top of your deck"). The owner
        # picks how many and which; discard->deck is logged like other recur moves.
        player = self.state.players[key]
        moved = 0
        for _ in range(action.count):
            matches = [c for c in player.discard if conditions.card_matches(c, action.selector)]
            if not matches:
                break
            card = self._pick_optional_from(key, matches, "target")
            if card is None:
                break  # owner declined to recur more ("up to")
            moved += 1
            player.discard.remove(card)
            player.deck.insert(0, card)  # top of deck (redraw next turn)
            self._log(
                gl.Bury(  # discard (public) -> deck: which card left discard is visible
                    t=self.state.turn_no, player=key, cards=[card.db_uuid], source="discard"
                )
            )
        if moved:
            self._run_on_discard_move(key)  # once per action, not per card

    def _act_play_extra_card(self, action: fx.PlayExtraCard, key: str) -> None:
        # Grant one more turn action this turn ("you may play an additional card").
        # Consumed by the turn loop; reset each turn. `order` (which kind) is not
        # enforced — the added action offers the normal playable set.
        player = self.state.players[key]
        player.flags["extra_plays"] = player.flags.get("extra_plays", 0) + 1

    def _act_remove_from_play(self, action: fx.RemoveFromPlay, key: str) -> None:
        # Board disruption: the ACTOR (key) sends up to `count` cards the target has
        # in play to the target's discard ("Discard 1 card your opponent has in
        # play" — Muay Thai Strikes / Jackhammer). The actor aims it via the
        # "target" decision point; both endpoints are public so the move is visible.
        if action.choose:
            self._remove_from_either_board(action.selector, action.count, key)
            return
        target = key if action.who is fx.Who.SELF else self.state.opponent_of(key)
        board = self.state.players[target].in_play
        for _ in range(action.count):
            matches = [c for c in board if conditions.card_matches(c, action.selector)]
            if not matches:
                return
            card = self._pick_from(key, matches, "target")
            board.remove(card)
            self.state.players[target].discard.append(card)
            self._log(
                gl.Discard(  # in_play (public) -> discard (public): a visible removal
                    t=self.state.turn_no, player=target, cards=[card.db_uuid], source="in_play"
                )
            )

    def _act_discard_in_play_match(self, action: fx.DiscardInPlayMatch, key: str) -> None:
        # Candyman Dan: discard 1 of the owner's own in-play cards (they choose), then
        # discard 1 of the OPPONENT's in-play cards of the SAME play order. No-op if the
        # owner has nothing in play; the second discard is skipped if the opponent has no
        # matching card.
        order = self._discard_one_in_play(key, key, fx.CardFilter())
        if order is None:
            return
        opp = self.state.opponent_of(key)
        self._discard_one_in_play(key, opp, fx.CardFilter(play_order=order))

    def _discard_one_in_play(self, actor: str, target: str, selector: fx.CardFilter) -> PlayOrder | None:
        """``actor`` picks and discards one of ``target``'s in-play cards matching
        ``selector`` (to ``target``'s discard). Returns the discarded card's play order,
        or ``None`` if none matched — the shared step of Candyman Dan's two-ended trade."""
        matches = [c for c in self.state.players[target].in_play if conditions.card_matches(c, selector)]
        if not matches:
            return None
        card = self._pick_from(actor, matches, "target")
        self.state.players[target].in_play.remove(card)
        self.state.players[target].discard.append(card)
        self._log(gl.Discard(t=self.state.turn_no, player=target, cards=[card.db_uuid], source="in_play"))
        return card.play_order

    def _act_return_to_hand(self, action: fx.ReturnToHand, key: str) -> None:
        # "Add `count` card(s) in play to their hand" (Fox Assassin V2): the ACTOR
        # (key) bounces matching in-play cards back to their OWNER's hand. `choose`
        # ranges the pick over BOTH boards ("any player has in play"); else `who`'s.
        boards = (
            [key, self.state.opponent_of(key)]
            if action.choose
            else [key if action.who is fx.Who.SELF else self.state.opponent_of(key)]
        )
        for _ in range(action.count):
            legal: list[Option] = []
            for b in boards:
                for c in self.state.players[b].in_play:
                    if conditions.card_matches(c, action.selector):
                        legal.append({**self._card_option(c), "owner": b})
            if not legal:
                break
            chosen = self._decide("return_to_hand", key, legal)
            owner = chosen["owner"]
            uuid = chosen["card"]
            board = self.state.players[owner].in_play
            card = next((c for c in board if c.db_uuid == uuid), None)
            if card is None:
                break
            board.remove(card)
            self.state.players[owner].hand.append(card)
            self._log(  # in_play (public) -> hand: which card left play is visible
                gl.Search(t=self.state.turn_no, player=owner, cards=[uuid], source="in_play")
            )

    def _act_flip(self, action: fx.Flip, key: str) -> None:
        target = key if action.who is fx.Who.SELF else self.state.opponent_of(key)
        player = self.state.players[target]
        flipped = player.deck[: action.n]
        del player.deck[: action.n]
        player.discard.extend(flipped)
        if flipped:
            self._log(
                gl.Discard(
                    t=self.state.turn_no,
                    player=target,  # whose deck was flipped (SELF or, e.g. Big Body Block, OPP)
                    cards=[c.db_uuid for c in flipped],
                    source="deck",  # flip: top of deck -> discard
                )
            )

    def _act_discard(self, action: fx.Discard, key: str) -> None:
        target = key if action.who is fx.Who.SELF else self.state.opponent_of(key)
        count = action.count
        if action.per is not None:
            count *= self._per_multiplier(action.per, action.per_who, key)
        if count:
            if self._suppresses_self_hand_loss(key, target):
                self._log_effect(key, "SuppressSelfHandLoss", target, {"n": count})
                return
            n = self._discard_from_hand(target, count, action.random, action.selector)
            if n > 0:
                self._run_on_bury(target, from_hand=True, is_discard=True)  # effect-caused hand discard (Tommy)

    def _act_reveal_and_discard(self, action: fx.RevealAndDiscard, key: str) -> None:
        # Reveal `count` random cards from the target's hand; discard the Stops among
        # them (Spin Wheel Kick). 0..count leave, so it is not a fixed-count discard.
        target = key if action.who is fx.Who.SELF else self.state.opponent_of(key)
        player = self.state.players[target]
        pool = list(player.hand)
        revealed: list[Card] = []
        for _ in range(min(action.count, len(pool))):
            card = self.state.rng.reveal(pool)
            pool.remove(card)
            revealed.append(card)
        dropped = [c for c in revealed if _is_stop_card(c)]
        for card in dropped:
            player.hand.remove(card)
        if dropped:
            player.discard.extend(dropped)
            self._log(
                gl.Discard(t=self.state.turn_no, player=target, cards=[c.db_uuid for c in dropped])
            )

    def _act_reveal_for_draw(self, action: fx.RevealForDraw, key: str) -> None:
        # "Your opponent randomly reveals `count` in their hand: draw `draw` for each
        # matched card" — a Stop (Bartholomew Hooke) or one whose move type equals the
        # actor's just-rolled skill (The Winning Ticket). Reveals stay in hand. The
        # rolled skill is populated by `_record_roll_ctx` before `OnRoll` fires.
        target = key if action.who is fx.Who.SELF else self.state.opponent_of(key)
        ctx = self._roll_ctx.get(key)
        rolled = ctx.skill if ctx is not None else None
        pool = list(self.state.players[target].hand)
        revealed: list[Card] = []
        for _ in range(min(action.count, len(pool))):
            card = self.state.rng.reveal(pool)
            pool.remove(card)
            revealed.append(card)
        hits = sum(1 for c in revealed if _reveal_matches(c, action.match_on, rolled))
        if revealed:
            self._log_effect(
                key,
                "RevealForDraw",
                target,
                {"revealed": [c.db_uuid for c in revealed], "hits": hits},
            )
        if hits > 0:
            self._draw(key, hits * action.draw, fx.DeckEnd.TOP)

    def _act_crowd(self, action: fx.CrowdMeter, key: str) -> None:
        self.state.crowd_meter += action.delta
        self._log(
            gl.CrowdMeter(t=self.state.turn_no, delta=action.delta, value=self.state.crowd_meter)
        )

    def _act_modify_roll(self, action: fx.ModifyRoll, key: str) -> None:
        target = key if action.who is fx.Who.SELF else self.state.opponent_of(key)
        slot = "this" if action.when is fx.RollWhen.THIS else "next"
        delta = action.delta
        if action.per is not None:
            # "+delta for each matching card in per_who's play" (Enjoy Everything).
            delta *= self._per_multiplier(action.per, action.per_who, key)
        self.state.players[target].pending_roll_mods[slot] += delta
        self._log_effect(key, "ModifyRoll", target, {"delta": delta, "when": slot})

    def _act_blank_gimmick(self, action: fx.BlankGimmick, key: str) -> None:
        # Executed (one-shot / non-Static) blank: latch the stored flag on the
        # target. A WHILE_IN_PLAY blank is normally authored as a Static effect and
        # read derived via GameState.is_gimmick_blanked (clears on breakout); this
        # path covers an OnHit "blank the gimmick" that fires once.
        target = key if action.who is fx.Who.SELF else self.state.opponent_of(key)
        player = self.state.players[target]
        player.gimmick_blanked = True
        # "...until their next turn" (Stiff Right Hand): mark it for the turn-boundary
        # sweep. Every other duration leaves the stored flag alone.
        if action.duration is fx.Duration.UNTIL_START_OF_YOUR_NEXT_TURN:
            player.blank_until_next_turn = self.state.turn_no
        self._log_effect(key, "BlankGimmick", target, {"duration": action.duration.value})

    def _act_flip_gimmick(self, action: fx.FlipGimmick, key: str) -> None:
        # Turn a competitor card to its back side (Copy Kat V2). One-way and
        # idempotent: latch the flip flag so the front's effects switch off and the
        # back's switch on (GimmickFlipped); re-firing on a later breakout is a no-op.
        target = key if action.who is fx.Who.SELF else self.state.opponent_of(key)
        player = self.state.players[target]
        if player.gimmick_flipped:
            return
        player.gimmick_flipped = True
        self._log_effect(key, "FlipGimmick", target, None)

    def _act_peek(self, action: fx.Peek, key: str) -> None:
        # Pure information: grant `key` a look at `target`'s hand for the rest of
        # this turn. No zone changes — observable() reads the peek flag to reveal
        # the hand (info model, #34/#38). Looking at your own hand is a no-op.
        target = key if action.who is fx.Who.SELF else self.state.opponent_of(key)
        if target == key:
            return
        self.state.players[key].flags["peek"] = {target: self.state.turn_no}
        self._log_effect(key, "Peek", target, {"hand_size": len(self.state.players[target].hand)})

    def _act_scry(self, action: fx.Scry, key: str) -> None:
        # Look at / reveal cards from the top (and/or bottom) of the target deck and
        # route them by value. The effect owner (`key`) is the actor: it keeps the
        # best cards, buries the worst on its own deck (or the best on an opponent's
        # — sabotage), and disposes of the rest per `rest`. Reveals are engine-side
        # (like _act_reveal_and_discard); `reveal` only controls log visibility.
        owner = key if action.deck is fx.Who.SELF else self.state.opponent_of(key)
        sabotage = owner != key
        deck = self.state.players[owner].deck

        revealed: list[Card] = []
        tn = min(max(action.top, 0), len(deck))
        revealed.extend(deck[:tn])
        del deck[:tn]
        bn = min(max(action.bottom, 0), len(deck))
        if bn:
            revealed.extend(deck[len(deck) - bn :])
            del deck[len(deck) - bn :]
        if not revealed:
            return

        seen = [c.db_uuid for c in revealed] if action.reveal else None
        self._log_effect(
            key, "Scry", owner, {"count": len(revealed), "revealed": seen, "public": action.reveal}
        )

        revealed.sort(key=_scry_value, reverse=True)  # best first

        take = min(max(action.to_hand, 0), len(revealed))
        if take:
            taken = revealed[:take]
            del revealed[:take]
            self.state.players[owner].hand.extend(taken)
            self._log(
                gl.Draw(
                    t=self.state.turn_no,
                    player=owner,
                    cards=[c.db_uuid for c in taken],
                    source="deck",
                    hidden=not action.reveal,
                )
            )

        b = min(max(action.bury, 0), len(revealed))
        if b:
            if sabotage:
                buried = revealed[:b]
                del revealed[:b]
            else:
                buried = revealed[len(revealed) - b :]
                del revealed[len(revealed) - b :]
            self._scry_to_bottom(owner, buried)

        self._scry_dispose(owner, revealed, action.rest, sabotage)
        self._hand_cap(owner)

    def _scry_dispose(
        self, owner: str, cards: list[Card], rest: fx.ScryRest, sabotage: bool
    ) -> None:
        # Route the leftovers: RETURN puts them all back on top (best on top of your
        # own deck, worst on top when sabotaging); CHOOSE keeps the valuable ones on
        # top and buries the junk (inverted when sabotaging).
        if not cards:
            return
        if rest is fx.ScryRest.RETURN:
            cards.sort(key=_scry_value, reverse=True)  # best first
            if sabotage:
                cards.reverse()
            self._scry_to_top(owner, cards)
        else:  # CHOOSE
            keep = [c for c in cards if (_scry_value(c) >= 2) != sabotage]
            junk = [c for c in cards if (_scry_value(c) >= 2) == sabotage]
            if junk:
                self._scry_to_bottom(owner, junk)
            self._scry_to_top(owner, keep)

    def _scry_to_top(self, owner: str, cards: list[Card]) -> None:
        # Put `cards` back on top of `owner`'s deck, cards[0] ending up topmost.
        deck = self.state.players[owner].deck
        for card in reversed(cards):
            deck.insert(0, card)

    def _scry_to_bottom(self, owner: str, cards: list[Card]) -> None:
        # Send `cards` to the bottom of `owner`'s deck and log the bury.
        if not cards:
            return
        self.state.players[owner].deck.extend(cards)
        self._log(
            gl.Bury(
                t=self.state.turn_no,
                player=owner,
                cards=[c.db_uuid for c in cards],
                source="deck",
            )
        )

    def _act_reveal_route(self, action: fx.RevealRoute, key: str) -> None:
        # Reveal the top card and route it by a runtime predicate: atk_type match ->
        # on_match, else on_fail. A `fail_optional` fail branch ("you may flip/bury")
        # is taken only when worthwhile — shed junk on your own deck, disrupt a
        # valuable card on an opponent's — otherwise the card is left on top.
        owner = key if action.deck is fx.Who.SELF else self.state.opponent_of(key)
        sabotage = owner != key
        deck = self.state.players[owner].deck
        if not deck:
            return
        # `CHOOSE` (top or bottom) is a blind pick — resolve it to the top.
        card = deck.pop() if action.reveal_from is fx.RevealFrom.BOTTOM else deck.pop(0)
        if action.match_parity is not None:
            matched = (card.number % 2 == 0) == action.match_parity
        else:
            matched = card.atk_type is action.match_atk
        self._log_effect(
            key,
            "RevealRoute",
            owner,
            {"card": card.db_uuid if action.reveal else None, "matched": matched},
        )
        if matched:
            dest = action.on_match
        elif action.fail_optional:
            worth = _scry_value(card) >= 2 if sabotage else _scry_value(card) < 2
            dest = action.on_fail if worth else fx.RevealDest.LEAVE
        else:
            dest = action.on_fail
        self._route_revealed(owner, card, dest)

    def _route_revealed(self, owner: str, card: Card, dest: fx.RevealDest) -> None:
        # Land a single revealed card in its chosen destination and log the move.
        player = self.state.players[owner]
        if dest is fx.RevealDest.HAND:
            player.hand.append(card)
            self._log(
                gl.Draw(
                    t=self.state.turn_no, player=owner, cards=[card.db_uuid], source="deck"
                )
            )
            self._hand_cap(owner)
        elif dest is fx.RevealDest.FLIP:
            player.discard.append(card)
            self._log(
                gl.Discard(
                    t=self.state.turn_no, player=owner, cards=[card.db_uuid], source="deck"
                )
            )
        elif dest is fx.RevealDest.BURY:
            player.deck.append(card)  # bottom
            self._log(
                gl.Bury(
                    t=self.state.turn_no, player=owner, cards=[card.db_uuid], source="deck"
                )
            )
        else:  # LEAVE
            player.deck.insert(0, card)  # back on top

    def _act_shuffle_hand_draw(self, action: fx.ShuffleHandDraw, key: str) -> None:
        # Shuffle a player's hand into their deck, shuffle, then draw `count` — a
        # mid-match hand refresh (Cyclone V2, on a bump). `choose` lets the actor
        # pick which player ("either player"); the default policy picks itself.
        if action.choose:
            target = self._decide_reshuffle_target(key)
        else:
            target = key if action.who is fx.Who.SELF else self.state.opponent_of(key)
        player = self.state.players[target]
        # None shuffles the WHOLE hand (Cyclone); Some(n) reveals and shuffles n chosen
        # cards (Memes Dealer). The public Bury (hand->deck) is the "reveal".
        if action.hand_count is None:
            shed = player.hand
            player.hand = []
        else:
            shed = self._pick_hand_cards(target, max(action.hand_count, 0))
        if shed:
            uuids = [c.db_uuid for c in shed]
            player.deck.extend(shed)
            self._log(
                gl.Bury(t=self.state.turn_no, player=target, cards=uuids, source="hand")
            )
        self._shuffle_deck(target)
        self._draw(target, max(action.count, 0))

    def _pick_hand_cards(self, target: str, n: int) -> list[Card]:
        """The owner of ``target`` reveals and removes up to ``n`` chosen cards from
        their hand (least valuable first, via the ``discard`` point) — the pick step of
        a partial hand shuffle (Memes Dealer)."""
        picked: list[Card] = []
        for _ in range(n):
            hand = self.state.players[target].hand
            if not hand:
                break
            card = self._pick_from(target, hand, "discard")
            hand.remove(card)
            picked.append(card)
        return picked

    def _decide_reshuffle_target(self, key: str) -> str:
        # "Either player" pick — the actor chooses itself or its opponent; the
        # default policy takes the first (itself).
        opp = self.state.opponent_of(key)
        legal = [{"kind": "seat", "seat": key}, {"kind": "seat", "seat": opp}]
        return self._decide("reshuffle_target", key, legal)["seat"]

    def _act_add_text_to_next(self, action: fx.AddTextToNext, key: str) -> None:
        """Queue a one-shot "added text" on ``who``'s next card matching ``selector``
        (the Madness trio). Held on the TARGET, so it outlives the source leaving play."""
        target = key if action.who is fx.Who.SELF else self.state.opponent_of(key)
        source = action.effects[0].raw_clause if action.effects else ""
        self.state.players[target].pending_text.append(
            PendingText(selector=action.selector, effects=action.effects, source=source)
        )
        self._log_effect(key, "AddTextToNext", target, {"text": source})

    def _apply_pending_text(self, key: str, card: Card) -> Card:
        """Consume any queued PendingText matching ``card`` and fold its effects onto
        the card instance, so the added text travels with it through the stop exchange
        and into play. Consumed on PLAY, whether or not the card is then stopped."""
        pend = self.state.players[key].pending_text
        hit = next((p for p in pend if conditions.card_matches(card, p.selector)), None)
        if hit is None:
            return card
        pend.remove(hit)
        card = replace(card, effects=tuple(card.effects) + tuple(hit.effects))
        self._log_effect(
            key, "AddTextToNext", key,
            {"card": card.db_uuid, "text": hit.source, "consumed": True},
        )
        return card

    def _act_choose_name(self, action: fx.ChooseName, key: str) -> None:
        """"Choose 1: <name>, <name>, or <name>" (Raven): bind one option for the rest
        of the match. The owner decides (a `name` decision point); the binding is read
        by ChosenNameIs, which gates the sibling effects referencing "that" name."""
        if not action.options:
            return
        legal = [{"kind": "name", "name": n} for n in action.options]
        chosen = self._decide("name", key, legal)
        self.state.players[key].chosen_name = chosen["name"]
        self._log_effect(key, "ChooseName", key, {"name": chosen["name"]})

    def _act_blank_stopped_text(self, action: fx.BlankStoppedText, key: str) -> None:
        """"The stopped card has blank text until the end of the turn": blank the card
        currently being stopped, by identity, for the rest of the turn. A no-op outside
        a stop exchange (no referent)."""
        if self._stopped_card is None:
            return
        self.state.blanked_text.add(self._stopped_card)
        self._log_effect(key, "BlankStoppedText", None, {"card": self._stopped_card})

    def _sweep_end_of_turn(self) -> None:
        """Drop everything scoped "until the end of the turn" by the turn just
        finished: timed buffs under UNTIL_END_OF_TURN and the per-card text blanks
        from BlankStoppedText. Runs with the other per-turn resets at the top of the
        following turn."""
        for player in self.state.players.values():
            player.timed_buffs = [
                b for b in player.timed_buffs if b.until is not fx.Duration.UNTIL_END_OF_TURN
            ]
        self.state.blanked_text.clear()

    def _sweep_next_turn_buffs(self, winner: str) -> None:
        """Sweep "until the start of your next turn" buffs now that the turn roll has
        named ``winner`` the active player.

        A turn is shared and its active player is only known once the roll resolves,
        so this cannot run before the roll — the buff therefore still feeds the roll
        that makes the turn yours, and dies immediately after (hand-adjudicated
        2026-07-20). ``granted_turn >= turn_no`` keeps a buff granted on THIS turn's
        roll alive; buffs on the non-active player are untouched, which is what lets
        one survive across every turn its owner does not win.
        """
        player = self.state.players[winner]
        player.timed_buffs = [
            b
            for b in player.timed_buffs
            if b.until is not fx.Duration.UNTIL_START_OF_YOUR_NEXT_TURN
            or b.granted_turn >= self.state.turn_no
        ]
        # Same boundary for a "blank until their next turn" poison (Stiff Right Hand).
        if (
            player.blank_until_next_turn is not None
            and player.blank_until_next_turn < self.state.turn_no
        ):
            player.blank_until_next_turn = None
            player.gimmick_blanked = False

    def _act_buff_skill(self, action: fx.BuffSkill, key: str) -> None:
        """Grant (or accumulate into) a TIMED skill buff; other durations are
        continuous (folded from the board) and are not executed as a mutation."""
        if action.duration not in (
            fx.Duration.UNTIL_END_OF_TURN,
            fx.Duration.UNTIL_START_OF_YOUR_NEXT_TURN,
        ):
            self._log_unsupported(
                key, repr(action), f"action {type(action).__name__} not modeled"
            )
            return
        target = key if action.who is fx.Who.SELF else self.state.opponent_of(key)
        player = self.state.players[target]
        cap = action.cap
        existing = next(
            (
                b
                for b in player.timed_buffs
                if b.source == self._clause
                and b.skill is action.skill
                and b.until is action.duration
            ),
            None,
        )
        if existing is None:
            delta = action.delta if cap is None else min(action.delta, cap)
            player.timed_buffs.append(
                TimedBuff(
                    skill=action.skill,
                    delta=delta,
                    until=action.duration,
                    source=self._clause,
                    cap=cap,
                    granted_turn=self.state.turn_no,
                )
            )
        else:
            total = existing.delta + action.delta
            existing.delta = total if cap is None else min(total, cap)
            delta = existing.delta
        self._log_effect(
            key,
            "BuffSkill",
            target,
            {
                "skill": action.skill.value,
                "delta": action.delta,
                "total": delta,
                "until": action.duration.value,
            },
        )

    def _act_choice(self, action: fx.Choice, key: str) -> None:
        # Pick exactly ONE branch of an "A or B" effect; the acting player decides
        # (a `choice` decision point), then that branch's actions resolve in order.
        options = action.options
        if not options:
            return
        legal = [
            {"kind": "choice", "index": i, "label": opt.label} for i, opt in enumerate(options)
        ]
        chosen = self._decide("choice", key, legal)
        for act in options[int(chosen["index"])].actions:
            self._apply_action(act, key)
            if self._resolve_pending():
                return

    def _act_win_tie(self, action: fx.WinTie, key: str) -> None:
        target = key if action.who is fx.Who.SELF else self.state.opponent_of(key)
        self.state.players[target].flags["win_tie"] = True
        self._log_effect(key, "WinTie", target, None)

    def _act_noop(self, action: fx.IRNode, key: str) -> None:
        """A passive marker read elsewhere (e.g. LowestRollWins at roll-off), not a
        mutation — folded like a Static buff, so executing it is a no-op, never
        ``Unsupported``."""

    def _act_lose_by(self, action: fx.LoseBy, key: str) -> None:
        loser = key if action.who is fx.Who.SELF else self.state.opponent_of(key)
        if action.kind is fx.LoseKind.DISQUALIFICATION and self._is_dq_immune(loser):
            # "no disqualifications" / "you cannot be disqualified": the loss is voided
            # and play continues (the triggering effect still fired).
            self._log_effect(key, "LoseByVoided", loser, {"kind": action.kind.value})
            return
        self._pending_loss = (loser, action.kind.value.lower())
        self._log_effect(key, "LoseBy", loser, {"kind": action.kind.value})

    def _is_dq_immune(self, loser: str) -> bool:
        """True iff ``loser`` is immune to a disqualification loss: some active
        ``DisqualificationRule`` disables DQ for them and none re-enables it. A rule
        applies when its scope is ``MATCH`` (any owner) or ``SELF`` (owner == loser).
        In-play-scoped and condition-gated. NOTE: last-played-order tie-break is task
        #93; with no re-enable card modeled yet this is exact."""
        disabled = False
        for owner, player in self.state.players.items():
            for effects, active in self.state._buff_sources(owner, player):
                if not active:
                    continue  # a blanked gimmick declares nothing
                for eff in effects:
                    if not isinstance(eff.trigger, fx.Static):
                        continue
                    for action in eff.actions:
                        if not isinstance(action, fx.DisqualificationRule):
                            continue
                        applies = action.scope is fx.DqScope.MATCH or owner == loser
                        if not applies or not conditions.holds(eff.condition, self.state, owner):
                            continue
                        if action.enabled:
                            return False  # an active rule re-enables DQ
                        disabled = True
        return disabled

    def _discard_from_hand(
        self, key: str, count: int, random: bool, selector: fx.CardFilter | None = None
    ) -> int:
        """Discard ``count`` cards from ``key``'s hand matching ``selector`` (``None`` =
        any). The hand's owner chooses which (via the ``discard`` decision point) even
        when an opponent forced it; a ``random`` discard draws from the seeded RNG
        instead. Runs out early if fewer than ``count`` matching cards exist (DESIGN.md §7)."""
        filt = selector if selector is not None else fx.CardFilter()
        player = self.state.players[key]
        dropped: list[Card] = []
        for _ in range(count):
            pool = [c for c in player.hand if conditions.card_matches(c, filt)]
            if not pool:
                break
            card = self.state.rng.reveal(pool) if random else self._choose_discard(key, pool)
            player.hand.remove(card)
            dropped.append(card)
        if dropped:
            player.discard.extend(dropped)
            self._log(
                gl.Discard(t=self.state.turn_no, player=key, cards=[c.db_uuid for c in dropped])
            )
        return len(dropped)

    def _choose_discard(self, key: str, pool: list[Card]) -> Card:
        legal = [self._discard_option(c) for c in pool]
        chosen = self._decide("discard", key, legal)
        return next(c for c in pool if c.db_uuid == chosen["card"])

    @staticmethod
    def _discard_option(card: Card) -> Option:
        return {
            "kind": "discard",
            "number": card.number,
            "card": card.db_uuid,
            "order": card.play_order.value,
        }

    # -- frequency guards --------------------------------------------------

    def _may_fire(self, eff: fx.Effect, key: str) -> bool:
        kind = eff.frequency.kind
        if kind is fx.Frequency.UNLIMITED:
            return True
        return self._freq_key(eff) not in self.state.players[key].freq_counters

    def _mark_fired(self, eff: fx.Effect, key: str) -> None:
        if eff.frequency.kind is not fx.Frequency.UNLIMITED:
            self.state.players[key].freq_counters[self._freq_key(eff)] = 1

    def _clear_turn_freq(self) -> None:
        for player in self.state.players.values():
            for name in [k for k in player.freq_counters if k.startswith("turn:")]:
                del player.freq_counters[name]

    @staticmethod
    def _freq_key(eff: fx.Effect) -> str:
        prefix = "turn:" if eff.frequency.kind is fx.Frequency.ONCE_PER_TURN else "match:"
        return prefix + eff.raw_clause + "|" + type(eff.trigger).__name__

    # -- outcome bookkeeping ----------------------------------------------

    def _resolve_pending(self) -> bool:
        if self._pending_loss is None:
            return False
        loser, reason = self._pending_loss
        self._pending_loss = None
        self._win(self.state.opponent_of(loser), reason)
        return True

    def _win(self, winner: str, reason: str) -> None:
        if self.result is None:
            self.result = GameResult(winner, reason, self.state.turn_no)

    def _ended(self) -> bool:
        return self.result is not None

    # -- logging helpers ---------------------------------------------------

    def _log(self, event: gl.Event) -> None:
        assert self.state.log is not None
        self.state.log.append(event)

    def _log_effect(self, src: str, action: str, target: str | None, detail: object) -> None:
        self._log(
            gl.EffectApplied(
                t=self.state.turn_no, src=src, action=action, target=target, detail=detail
            )
        )

    def _log_unsupported(self, owner: str, raw: str, reason: str) -> None:
        self._log(gl.Unsupported(t=self.state.turn_no, owner=owner, raw=raw, reason=reason))

    # -- policy / options --------------------------------------------------

    def _decide(self, point: str, key: str, legal: list[Option]) -> Option:
        if len(legal) == 1:
            return legal[0]
        chosen = self.policies[key].choose(point, legal, self.state, key)
        self._log(
            gl.Decision(
                t=self.state.turn_no,
                player=key,
                point=point,
                legal=legal,
                chosen=chosen,
                policy=self.policies[key].name,
            )
        )
        return chosen

    def _playable_options(self, key: str) -> list[Option]:
        chain = self.state.players[key].in_play
        return [
            self._card_option(c)
            for c in self.state.players[key].hand
            if _playable(chain, c) or self._also_lead_now(key, c)
        ]

    def _also_lead_now(self, key: str, card: Card) -> bool:
        """Whether ``card`` may be played as a Lead this instant via an ``AlsoLead``
        self-declaration whose condition currently holds ("… this card is also a
        Lead"). Lets an otherwise-ungated Finish/Follow-Up start a chain."""
        return any(
            isinstance(a, fx.AlsoLead) and conditions.holds(a.condition, self.state, key)
            for eff in card.effects
            for a in eff.actions
        )

    @staticmethod
    def _card_option(card: Card) -> Option:
        return {
            "kind": "play",
            "number": card.number,
            "card": card.db_uuid,
            "order": card.play_order.value,
            "atk_type": card.atk_type.value,
        }

    def _take_from_hand(self, key: str, number: int) -> Card:
        hand = self.state.players[key].hand
        card = next(c for c in hand if c.number == number)
        hand.remove(card)
        return card


_SKILL_ORDER = list(Skill)


def _rolled_set_key(eff: fx.Effect) -> str:
    """The ``freq_counters`` key holding an ``OnRolledAll`` effect's rolled-skill bitmask.
    A distinct ``rollset:`` namespace, so the per-turn / per-match frequency sweeps leave
    it alone — the set accumulates across turns until the gimmick fires."""
    return f"rollset:{eff.raw_clause}"


def _skill_bit(skill: Skill) -> int:
    """A single-bit mask for ``skill`` (its index in the canonical skill order)."""
    return 1 << _SKILL_ORDER.index(skill)


def _attacker_meets_tag_gates(eff: fx.Effect, attack: Card) -> bool:
    """Whether ``attack`` satisfies every ``StopRequiresTag`` gate in a stop ``eff`` —
    a passive marker paired with a sibling ``Stop``, requiring the attacked card carry
    the named tag ("Stop any Grapple with a Spotlight"). No gate ⇒ always true."""
    return all(
        attack.tags.__contains__(a.tag)
        for a in eff.actions
        if isinstance(a, fx.StopRequiresTag)
    )


def _is_stop_card(card: Card) -> bool:
    """Whether ``card`` can act as a Stop — carries at least one ``Stop`` action (its
    online condition is not checked; a revealed Stop is discarded regardless)."""
    return any(isinstance(a, fx.Stop) for eff in card.effects for a in eff.actions)


def _reveal_matches(card: Card, match_on: fx.RevealMatch, rolled: Skill | None) -> bool:
    """Whether a card revealed by :meth:`_act_reveal_for_draw` counts toward the draw:
    a Stop (``STOP``), or one whose move type equals the actor's rolled skill
    (``ROLLED_SKILL``; no match when the actor did not roll a move skill)."""
    if match_on is fx.RevealMatch.STOP:
        return _is_stop_card(card)
    return rolled is not None and _atk_type_matches_skill(card.atk_type, rolled)


def _atk_type_matches_skill(atk: AtkType, skill: Skill) -> bool:
    """True iff a card's attack (move) type is the same move as ``skill`` — one of
    Strike/Grapple/Submission and matching. ``AtkType.NONE`` and the non-move skills
    (Power/Agility/Technique) never match."""
    return atk is not AtkType.NONE and atk.value == skill.value


def _scry_value(card: Card) -> int:
    """Value a scried card by how much the actor wants it kept/drawn: a Finish (a win
    condition) over a stop (defense) over a plain card. Mirrors the discard-recycle
    read so scry keeps the deck's best on top / in hand."""
    if card.play_order is PlayOrder.FINISH:
        return 3
    if _is_stop_card(card):
        return 2
    return 1


def _is_unstoppable_by(attack: Card, stopper: Card) -> bool:
    """Whether ``attack`` declares itself ``Unstoppable`` against ``stopper`` — i.e. it
    carries an :class:`fx.Unstoppable` whose ``by_order`` is the stopper's play order
    (or ``None`` = unstoppable by anything). "Cannot be stopped by Follow Ups"."""
    return any(
        isinstance(action, fx.Unstoppable)
        and (action.by_order is None or action.by_order is stopper.play_order)
        for eff in attack.effects
        for action in eff.actions
    )


def _playable(board: list[Card], card: Card) -> bool:
    """Whether ``card`` is a legal play given the player's own persistent in-play
    board (DESIGN.md §6, order-only chain): a Lead is always playable (you may
    stack another); a Follow Up needs a Lead in play; a Finish needs a Follow Up
    in play. Type is irrelevant to the chain — it only matters for stops.
    """
    order = card.play_order
    if order is PlayOrder.LEAD:
        return True
    if order is PlayOrder.FOLLOWUP:
        return any(c.play_order is PlayOrder.LEAD for c in board)
    if order is PlayOrder.FINISH:
        return any(c.play_order is PlayOrder.FOLLOWUP for c in board)
    return False  # PlayOrder.NONE cards aren't played as attacks


# Action dispatch table (bound methods resolved on the instance at call time).
_ACTIONS: dict[type, Callable[[Engine, Any, str], None]] = {
    fx.Draw: Engine._act_draw,
    fx.Bury: Engine._act_bury,
    fx.Flip: Engine._act_flip,
    fx.Discard: Engine._act_discard,
    fx.CrowdMeter: Engine._act_crowd,
    fx.ModifyRoll: Engine._act_modify_roll,
    fx.WinTie: Engine._act_win_tie,
    fx.BlankGimmick: Engine._act_blank_gimmick,
    fx.FlipGimmick: Engine._act_flip_gimmick,
    fx.LoseBy: Engine._act_lose_by,
    fx.DisqualificationRule: Engine._act_noop,  # Static, read via _is_dq_immune; never executed
    fx.ConsideredCompare: Engine._act_noop,  # Static, read in conditions.holds; never executed
    fx.SuppressOpponentDraw: Engine._act_noop,  # Static, read in _act_draw; never executed
    fx.SuppressSelfHandLoss: Engine._act_noop,  # Static, read at the hand-loss points
    fx.BumpDrawReplace: Engine._act_noop,  # Static, read in _do_bump; never executed
    fx.LowestRollWins: Engine._act_noop,
    fx.FlipGimmickSigns: Engine._act_noop,
    fx.CountsAsInPlay: Engine._act_noop,  # Static, read via count_in_play; never executed
    fx.ElectBumpOnSameSkill: Engine._act_noop,  # Static, read in the roll-off; never executed
    fx.SwitchRolledSkill: Engine._act_noop,  # Static, read in both roll paths; never executed
    fx.AddText: Engine._act_noop,  # Static, read via _injected_text at play time; never executed
    fx.StopRequiresTag: Engine._act_noop,  # marker, read via _card_can_stop; never executed
    fx.BlankText: Engine._act_noop,  # Static, read via is_text_blanked; never executed
    fx.BlankStoppedText: Engine._act_blank_stopped_text,
    fx.ChooseName: Engine._act_choose_name,
    fx.AddTextToNext: Engine._act_add_text_to_next,
    fx.Reroll: Engine._act_reroll,  # THIS: structural no-op; NEXT: grants a next-turn re-roll
    fx.Unstoppable: Engine._act_noop,  # Static, read via _is_unstoppable_by; never executed
    fx.AlsoLead: Engine._act_noop,  # Static, read via _also_lead_now; never executed
    fx.StopCountsOrderAs: Engine._act_noop,  # Static, read via _card_can_stop; never executed
    fx.SuppressStop: Engine._act_noop,  # Static, read via _card_can_stop; never executed
    fx.DoubleFinishIfBumped: Engine._act_noop,  # Static, read in the finish sequence
    fx.RevealAndDiscard: Engine._act_reveal_and_discard,
    fx.RevealForDraw: Engine._act_reveal_for_draw,
    fx.MaxHandSize: Engine._act_noop,  # Static, read via effective_hand_cap; never executed
    fx.MinHandSize: Engine._act_noop,  # Static, read via effective_hand_cap; never executed
    fx.MirrorOpponentIncrease: Engine._act_noop,  # Static, read via effective_stats; never executed
    fx.ShuffleDeck: Engine._act_shuffle_deck,
    fx.Search: Engine._act_search,
    fx.ShuffleIntoDeck: Engine._act_shuffle_into_deck,
    fx.AddFromDiscard: Engine._act_add_from_discard,
    fx.SwapHandDiscard: Engine._act_swap_hand_discard,
    fx.RecurToDeckTop: Engine._act_recur_to_deck_top,
    fx.RemoveFromPlay: Engine._act_remove_from_play,
    fx.DiscardInPlayMatch: Engine._act_discard_in_play_match,
    fx.ReturnToHand: Engine._act_return_to_hand,
    fx.PlayExtraCard: Engine._act_play_extra_card,
    fx.Peek: Engine._act_peek,
    fx.Scry: Engine._act_scry,
    fx.RevealRoute: Engine._act_reveal_route,
    fx.ShuffleHandDraw: Engine._act_shuffle_hand_draw,
    fx.BuffSkill: Engine._act_buff_skill,
    fx.Choice: Engine._act_choice,
}
