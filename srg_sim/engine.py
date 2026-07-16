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
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

from srg_sim import conditions
from srg_sim import effects as fx
from srg_sim import gamelog as gl
from srg_sim.cards import AtkType, Card, Deck, PlayOrder, Skill
from srg_sim.finish import is_auto_success, stat_breaks_out
from srg_sim.rng import SeededRNG
from srg_sim.state import GameState, PlayerState

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
        """Match setup: StartOfMatch effects, shuffle, opening hands, mulligans."""
        for key in ("A", "B"):
            self._run_effects(self._standing_effects(key), fx.StartOfMatch, key)
        for key in ("A", "B"):
            self.state.rng.shuffle(self.state.players[key].deck)
        for key in ("A", "B"):
            self._draw(key, OPENING_HAND)
        for key in ("A", "B"):
            self._mulligan(key)

    def _mulligan(self, key: str) -> None:
        # First-turn option (DESIGN.md §6): only with NO Leads in hand, a player
        # may randomly bury the hand to the bottom of the deck and redraw the same
        # number. With a Lead in hand the option is not offered.
        player = self.state.players[key]
        if not player.hand or any(c.play_order is PlayOrder.LEAD for c in player.hand):
            return
        legal: list[Option] = [{"kind": "redraw"}, {"kind": "keep"}]
        if self._decide("mulligan", key, legal)["kind"] != "redraw":
            return
        buried = list(player.hand)
        self.state.rng.shuffle(buried)  # randomly buried
        player.hand.clear()
        player.deck.extend(buried)  # to the bottom of the deck
        self._log(
            gl.Bury(
                t=self.state.turn_no,
                player=key,
                cards=[c.db_uuid for c in buried],
                source="hand",
                hidden=True,  # hand -> deck: both private, opponent can't track which
            )
        )
        self._draw(key, len(buried))

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
        winner = self._turn_roll()
        if self._ended() or not self._draw_for_turn(winner):
            return
        self._take_turn_action(winner)  # play ONE card (or pass+bury); the board persists

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
        self._run_on_roll("A")
        self._run_on_roll("B")
        return winner

    def _run_on_bump(self) -> None:
        """Fire both players' ``OnBump`` effects for a bump just taken (both sides
        bump on a tie). A once-per-turn frequency guard keeps a bump-punish gimmick
        firing only once even when a roll ties repeatedly in one turn."""
        for key in ("A", "B"):
            self._run_effects(self._standing_effects(key), fx.OnBump, key)

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

    # -- roll-off ----------------------------------------------------------

    def _roll_off(self) -> str:
        lowest = self._lowest_wins()
        sa, va = self._roll_for("A", use_pending=True)
        sb, vb = self._roll_for("B", use_pending=True)
        self._consume_pending()
        bumps = 0
        while va == vb and bumps < MAX_TIE_REROLLS:
            forced = self._tie_winner()
            if forced is not None:
                self._record_roll_ctx(sa, va, sb, vb)
                self._log(gl.TurnResult(t=self.state.turn_no, winner=forced, tie_bumps=bumps))
                return forced
            self._draw("A", 1)  # bump: both players draw, then re-roll (mechanics §2)
            self._draw("B", 1)
            bumps += 1
            self._run_on_bump()  # bump-punish gimmicks (Mastermind: opp next roll -2)
            sa, va = self._roll_for("A", use_pending=False)
            sb, vb = self._roll_for("B", use_pending=False)
        winner = self._roll_winner(va, vb, lowest)
        self._record_roll_ctx(sa, va, sb, vb)
        self._log(gl.TurnResult(t=self.state.turn_no, winner=winner, tie_bumps=bumps))
        return winner

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
            "A": conditions.RollContext(skill=sa, gap=vb - va),
            "B": conditions.RollContext(skill=sb, gap=va - vb),
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

    def _consume_pending(self) -> None:
        for player in self.state.players.values():
            mods = player.pending_roll_mods
            mods["this"], mods["next"] = mods["next"], 0

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
        self._log(
            gl.Play(
                t=self.state.turn_no,
                player=active,
                card=card.db_uuid,
                order=card.play_order.value,
                atk_type=card.atk_type.value,
            )
        )
        self._run_effects(card.effects, fx.OnPlay, active)
        if self._ended():
            return False
        stop = self._offer_stop(defender, active, card)
        if stop is not None:
            self._apply_stop(active, defender, card, stop)
            return False
        self.state.players[active].in_play.append(card)
        self._run_effects(card.effects, fx.OnHit, active)
        self._enforce_hand_caps()  # a new Static max-handsize mod may force a discard
        return not self._ended()

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
        gates all fall out of the condition). Cards with no Stop effect cannot stop.
        """
        return any(
            isinstance(action, fx.Stop)
            and _stop_matches(action, attack)
            and conditions.holds(eff.condition, self.state, defender)
            for eff in stopper.effects
            for action in eff.actions
        )

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
        self._run_effects(attack.effects, fx.OnStop, active)
        self._run_effects(stop.effects, fx.OnStop, defender)

    # -- finish sequence + breakout ---------------------------------------

    def _finish_sequence(self, finisher: str, defender: str, card: Card) -> None:
        skill = self.state.rng.roll()
        base = self._stat(finisher, skill)
        # The whole in-play combo pays off: sum every card's printed bonus for the
        # rolled skill, plus any flat "+N to your Finish rolls" (DESIGN.md §5).
        bonus = sum(c.bonus_for(skill) for c in self.state.players[finisher].in_play)
        bonus += self._finish_roll_bonus(finisher)
        cm = self.state.crowd_meter
        value = base + bonus + cm
        auto = is_auto_success(value, cm)
        self._log_finish_attempt(finisher, card, skill, bonus, value, cm, auto)
        if not auto and self._breakout(defender, value):
            self._on_broken_out(finisher)  # defender broke out; the match resumes
            return
        self._win(finisher, "finish")

    def _finish_roll_bonus(self, key: str) -> int:
        """Flat any-skill "+N to your Finish rolls" from the finisher's live effects
        (in-play combo, gimmick, entrance), each gated by its condition (DESIGN.md §5)."""
        total = 0
        for eff in self._standing_effects(key):
            if conditions.holds(eff.condition, self.state, key):
                total += sum(a.delta for a in eff.actions if isinstance(a, fx.FinishRollBonus))
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

    def _breakout(self, defender: str, finish_value: int) -> bool:
        cm = self.state.crowd_meter
        rolls: list[gl.BreakoutRoll] = []
        broke = False
        for _ in range(BREAKOUT_ATTEMPTS):
            skill = self.state.rng.roll()
            val = self._stat(defender, skill)
            success = stat_breaks_out(val, finish_value, 0, cm)
            rolls.append(gl.BreakoutRoll(skill=skill.value, value=val, penalty=0, success=success))
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
        player = self.state.players[key]
        out: list[fx.Effect] = []
        if not player.gimmick_blanked:
            out.extend(player.competitor.effects)
        out.extend(player.entrance.effects)
        for card in player.in_play:
            out.extend(card.effects)
        return tuple(out)

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
        if self._may_fire(eff, key) and conditions.holds(eff.condition, self.state, key, roll):
            self._mark_fired(eff, key)
            self._apply_actions(eff, key)

    def _apply_actions(self, eff: fx.Effect, key: str) -> None:
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

    def _act_draw(self, action: fx.Draw, key: str) -> None:
        target = key if action.who is fx.Who.SELF else self.state.opponent_of(key)
        self._draw(target, action.n, action.source)

    def _act_shuffle_deck(self, action: fx.ShuffleDeck, key: str) -> None:
        target = key if action.who is fx.Who.SELF else self.state.opponent_of(key)
        self.state.rng.shuffle(self.state.players[target].deck)
        self._log_effect(key, "ShuffleDeck", target, None)

    def _act_bury(self, action: fx.Bury, key: str) -> None:
        target = key if action.who is fx.Who.SELF else self.state.opponent_of(key)
        cards = list(self.state.players[target].discard[: action.count])
        if action.random:
            self.state.rng.shuffle(cards)
        if cards:
            self._bury_cards(target, cards)

    def _act_search(self, action: fx.Search, key: str) -> None:
        # Tutor: pull the first deck card matching the filter into hand, then
        # shuffle the deck (you looked through it). dest is HAND (the only Dest);
        # "put it on top" tutors are modelled as into-hand — a close, stronger
        # approximation. A no-match search just shuffles.
        player = self.state.players[key]
        match = next((c for c in player.deck if conditions.card_matches(c, action.filter)), None)
        if match is not None:
            player.deck.remove(match)
            player.hand.append(match)
            self._log(
                gl.Search(
                    t=self.state.turn_no,
                    player=key,
                    cards=[match.db_uuid],
                    source="deck",
                    hidden=True,  # deck -> hand: both private, opponent sees only counts
                )
            )
        self.state.rng.shuffle(player.deck)
        self._hand_cap(key)

    def _act_shuffle_into_deck(self, action: fx.ShuffleIntoDeck, key: str) -> None:
        # Recur ONE matching card from discard into the deck, then shuffle. The IR
        # node has no count, so "shuffle 2 / up to 3 cards" is authored as repeated
        # ShuffleIntoDeck actions (no IR change; DESIGN.md §3 review gate).
        player = self.state.players[key]
        match = next(
            (c for c in player.discard if conditions.card_matches(c, action.selector)), None
        )
        if match is not None:
            player.discard.remove(match)
            player.deck.append(match)
            self._log(
                gl.Bury(  # discard -> deck movement (the shuffle rides the RNG state)
                    t=self.state.turn_no, player=key, cards=[match.db_uuid], source="discard"
                )
            )
        self.state.rng.shuffle(player.deck)

    def _act_add_from_discard(self, action: fx.AddFromDiscard, key: str) -> None:
        # Recur ONE matching card from discard straight to hand ("add 1 <type>
        # from your discard pile to your hand").
        player = self.state.players[key]
        match = next((c for c in player.discard if conditions.card_matches(c, action.filter)), None)
        if match is None:
            return
        player.discard.remove(match)
        player.hand.append(match)
        self._log(
            gl.Search(  # discard (public) -> hand: which card left discard is visible
                t=self.state.turn_no, player=key, cards=[match.db_uuid], source="discard"
            )
        )
        self._hand_cap(key)

    def _act_flip(self, action: fx.Flip, key: str) -> None:
        player = self.state.players[key]
        flipped = player.deck[: action.n]
        del player.deck[: action.n]
        player.discard.extend(flipped)
        if flipped:
            self._log(
                gl.Discard(
                    t=self.state.turn_no,
                    player=key,
                    cards=[c.db_uuid for c in flipped],
                    source="deck",  # flip: top of deck -> discard
                )
            )

    def _act_discard(self, action: fx.Discard, key: str) -> None:
        target = key if action.who is fx.Who.SELF else self.state.opponent_of(key)
        self._discard_from_hand(target, action.count, action.random)

    def _act_crowd(self, action: fx.CrowdMeter, key: str) -> None:
        self.state.crowd_meter += action.delta
        self._log(
            gl.CrowdMeter(t=self.state.turn_no, delta=action.delta, value=self.state.crowd_meter)
        )

    def _act_modify_roll(self, action: fx.ModifyRoll, key: str) -> None:
        target = key if action.who is fx.Who.SELF else self.state.opponent_of(key)
        slot = "this" if action.when is fx.RollWhen.THIS else "next"
        self.state.players[target].pending_roll_mods[slot] += action.delta
        self._log_effect(key, "ModifyRoll", target, {"delta": action.delta, "when": slot})

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
        self._pending_loss = (loser, action.kind.value.lower())
        self._log_effect(key, "LoseBy", loser, {"kind": action.kind.value})

    def _discard_from_hand(self, key: str, count: int, random: bool) -> None:
        """Discard ``count`` cards from ``key``'s hand. The hand's owner chooses which
        (via the ``discard`` decision point) even when an opponent forced it; a
        ``random`` discard draws from the seeded RNG instead (DESIGN.md §7)."""
        player = self.state.players[key]
        dropped: list[Card] = []
        for _ in range(count):
            if not player.hand:
                break
            card = self.state.rng.reveal(player.hand) if random else self._choose_discard(key)
            player.hand.remove(card)
            dropped.append(card)
        if dropped:
            player.discard.extend(dropped)
            self._log(
                gl.Discard(t=self.state.turn_no, player=key, cards=[c.db_uuid for c in dropped])
            )

    def _choose_discard(self, key: str) -> Card:
        hand = self.state.players[key].hand
        legal = [self._discard_option(c) for c in hand]
        chosen = self._decide("discard", key, legal)
        return next(c for c in hand if c.db_uuid == chosen["card"])

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
        return [self._card_option(c) for c in self.state.players[key].hand if _playable(chain, c)]

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


def _stop_matches(stop: fx.Stop, attack: Card) -> bool:
    """Whether a ``Stop`` action's order/type filter covers this attack (None = any)."""
    if stop.order is not None and stop.order is not attack.play_order:
        return False
    return stop.atk_type is None or stop.atk_type is attack.atk_type


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
    fx.LoseBy: Engine._act_lose_by,
    fx.LowestRollWins: Engine._act_noop,
    fx.MaxHandSize: Engine._act_noop,  # Static, read via effective_hand_cap; never executed
    fx.ShuffleDeck: Engine._act_shuffle_deck,
    fx.Search: Engine._act_search,
    fx.ShuffleIntoDeck: Engine._act_shuffle_into_deck,
    fx.AddFromDiscard: Engine._act_add_from_discard,
}
