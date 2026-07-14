"""Turn loop, effect executor, stop resolution, finish sequence (DESIGN.md §6).

The engine plays two :class:`~srg_sim.cards.Deck` s against two
:class:`~srg_sim.policy.Policy` policies under one seed and produces a complete
JSONL :class:`~srg_sim.gamelog.GameLog` plus a :class:`GameResult`. Everything
non-deterministic flows through the shared :class:`~srg_sim.rng.SeededRNG`, so
``Engine(...).play()`` is a pure function of ``(decks, policies, seed)`` —
re-running reproduces a byte-identical log (DESIGN.md §8 replay).

**M1 scope & simplifications** (each honest, none silent). A won turn resolves
one *within-turn* combo (Lead → Followup* → optional Finish); the combo does not
persist across turns, so cross-turn board state is out of scope (DESIGN.md §12).
Rolls and breakouts use **actual seeded draws** (a face is drawn, its value is the
derived stat), sharing the exact per-face rules ported into
:mod:`srg_sim.finish`. Stops are RPS-by-attack-type, with the skill-stop Follow
Ups (cards 13/14/15) additionally gated by :func:`srg_sim.stops.evaluate_stop`.
The executor applies a focused action set; any effect it cannot apply — an
``Unsupported`` node or an unhandled action — is emitted as an ``unsupported``
log event, never dropped. Conditional ``Static`` buffs are not folded in yet.
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
from srg_sim.stops import STOP_CARDS, evaluate_stop

if TYPE_CHECKING:
    from srg_sim.policy import Option, Policy

OPENING_HAND = 3
HAND_CAP = 10
BREAKOUT_ATTEMPTS = 3
TURN_CAP = 400
MAX_TIE_REROLLS = 64

# RPS: a beats b. Strike ▷ Grapple ▷ Submission ▷ Strike (DESIGN.md §2).
_BEATS = {
    AtkType.STRIKE: AtkType.GRAPPLE,
    AtkType.GRAPPLE: AtkType.SUBMISSION,
    AtkType.SUBMISSION: AtkType.STRIKE,
}
_SKILL_STOP_NUMBERS = {card for card, _ in STOP_CARDS.values()}


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
    ) -> None:
        rng = SeededRNG(seed)
        self.decks = {"A": deck_a, "B": deck_b}
        self.policies: dict[str, Policy] = {"A": policy_a, "B": policy_b}
        self.state = GameState(players=self._build_players(), rng=rng)
        self.state.log = gl.GameLog(header=self._header(seed, created))
        self.result: GameResult | None = None
        self._pending_loss: tuple[str, str] | None = None

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
            kind="sim",
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
        winner = self._roll_off()
        self.state.active = winner
        loser = self.state.opponent_of(winner)
        self._run_effects(self._standing_effects(winner), fx.OnWinTurn, winner)
        self._run_effects(self._standing_effects(loser), fx.OnLoseTurn, loser)
        if self._ended() or not self._draw_for_turn(winner):
            return
        self._attack_sequence(winner)
        if self._ended():
            return
        self._hand_cap(winner)
        self._cleanup_in_play(winner)

    # -- roll-off ----------------------------------------------------------

    def _roll_off(self) -> str:
        va = self._roll_for("A", use_pending=True)
        vb = self._roll_for("B", use_pending=True)
        self._consume_pending()
        bumps = 0
        while va == vb and bumps < MAX_TIE_REROLLS:
            forced = self._tie_winner()
            if forced is not None:
                self._log(gl.TurnResult(t=self.state.turn_no, winner=forced, tie_bumps=bumps))
                return forced
            bumps += 1
            va = self._roll_for("A", use_pending=False)
            vb = self._roll_for("B", use_pending=False)
        winner = "A" if va >= vb else "B"
        self._log(gl.TurnResult(t=self.state.turn_no, winner=winner, tie_bumps=bumps))
        return winner

    # -- derived stats (with live condition evaluation) --------------------

    def _holds(self, key: str) -> Callable[[fx.Condition], bool]:
        """A condition evaluator bound to ``key`` (resolves conditional buffs/stops)."""
        return lambda cond: conditions.holds(cond, self.state, key)

    def _stats(self, key: str) -> dict[str, int]:
        return self.state.effective_stats(key, self._holds(key))

    def _stat(self, key: str, skill: Skill) -> int:
        return self.state.effective_stat(key, skill, self._holds(key))

    def _roll_for(self, key: str, use_pending: bool) -> int:
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
        return value

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
                )
            )

    # -- attack sequence ---------------------------------------------------

    def _attack_sequence(self, active: str) -> None:
        defender = self.state.opponent_of(active)
        legal = self._playable_options(active) + [{"kind": "pass"}]
        choice = self._decide("turn_action", active, legal)
        if choice["kind"] == "pass":
            self._do_pass(active)
            return
        card = self._take_from_hand(active, choice["number"])
        self._play_chain(active, defender, card)

    def _play_chain(self, active: str, defender: str, card: Card) -> None:
        while True:
            if not self._resolve_play(active, defender, card):
                return  # stopped -> chain broken
            if self._ended():
                return
            if card.play_order is PlayOrder.FINISH:
                self._finish_sequence(active, defender, card)
                return
            nxt = self._continue_choice(active)
            if nxt is None:
                return
            card = self._take_from_hand(active, nxt["number"])

    def _continue_choice(self, active: str) -> Option | None:
        plays = self._playable_options(active)
        if not plays:
            return None
        legal = plays + [{"kind": "stop_chain"}]
        choice = self._decide("continue", active, legal)
        return None if choice["kind"] == "stop_chain" else choice

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
        return not self._ended()

    def _offer_stop(self, defender: str, attacker: str, card: Card) -> Card | None:
        stops = self._legal_stops(defender, attacker, card)
        if not stops:
            return None
        legal = [{"kind": "none"}] + [self._card_option(c) for c in stops]
        choice = self._decide("stop", defender, legal)
        if choice["kind"] == "none":
            return None
        return self._take_from_hand(defender, choice["number"])

    def _legal_stops(self, defender: str, attacker: str, card: Card) -> list[Card]:
        return [
            c
            for c in self.state.players[defender].hand
            if beats(card.atk_type, c.atk_type) and self._skill_stop_ok(defender, attacker, card, c)
        ]

    def _skill_stop_ok(self, defender: str, attacker: str, attack: Card, stop: Card) -> bool:
        """Skill-stop Follow Ups (13/14/15) also need their stop to be online (§6)."""
        if stop.number not in _SKILL_STOP_NUMBERS:
            return True
        result = evaluate_stop(
            self._stats(defender),
            attack.atk_type.value,
            self._stats(attacker),
        )
        return result["online"]

    def _apply_stop(self, active: str, defender: str, attack: Card, stop: Card) -> None:
        self.state.players[active].discard.append(attack)
        self.state.players[defender].discard.append(stop)
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
        bonus = card.bonus_for(skill)
        cm = self.state.crowd_meter
        value = base + bonus + cm
        auto = is_auto_success(value, cm)
        self._log_finish_attempt(finisher, card, skill, bonus, value, cm, auto)
        if not auto and self._breakout(defender, value):
            self._on_broken_out(finisher)  # defender broke out; the match resumes
            return
        self._win(finisher, "finish")

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
        self._discard_in_play(finisher)
        self.state.crowd_meter += 1
        self._log(gl.CrowdMeter(t=self.state.turn_no, delta=1, value=self.state.crowd_meter))

    # -- end of turn -------------------------------------------------------

    def _hand_cap(self, key: str) -> None:
        hand = self.state.players[key].hand
        excess = len(hand) - HAND_CAP
        if excess <= 0:
            return
        dropped = hand[:excess]
        del hand[:excess]
        self.state.players[key].discard.extend(dropped)
        self._log(gl.Discard(t=self.state.turn_no, player=key, cards=[c.db_uuid for c in dropped]))

    def _cleanup_in_play(self, key: str) -> None:
        self._discard_in_play(key)

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
        self, effects: tuple[fx.Effect, ...], trigger: type[fx.IRNode], key: str
    ) -> None:
        for eff in effects:
            if isinstance(eff.trigger, trigger) and self._may_fire(eff, key):
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
        self._draw(key, action.n, action.source)

    def _act_bury(self, action: fx.Bury, key: str) -> None:
        target = key if action.who is fx.Who.SELF else self.state.opponent_of(key)
        cards = list(self.state.players[target].discard[: action.count])
        if action.random:
            self.state.rng.shuffle(cards)
        if cards:
            self._bury_cards(target, cards)

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
        self._move_from_hand(key, action.count, "discard", gl.Discard)

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

    def _act_lose_by(self, action: fx.LoseBy, key: str) -> None:
        loser = key if action.who is fx.Who.SELF else self.state.opponent_of(key)
        self._pending_loss = (loser, action.kind.value.lower())
        self._log_effect(key, "LoseBy", loser, {"kind": action.kind.value})

    def _move_from_hand(
        self, key: str, count: int, zone: str, event: type[gl._CardMovement]
    ) -> None:
        player = self.state.players[key]
        moved = player.hand[:count]
        del player.hand[:count]
        getattr(player, zone).extend(moved)
        if moved:
            self._log(event(t=self.state.turn_no, player=key, cards=[c.db_uuid for c in moved]))

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


def _playable(chain: list[Card], card: Card) -> bool:
    """Whether ``card`` is a legal next link given the current combo ``chain``."""
    order = card.play_order
    if order is PlayOrder.NONE:
        return False
    if not chain:
        return order is PlayOrder.LEAD
    top = chain[-1].play_order
    if top is PlayOrder.FINISH:
        return False  # a completed combo takes no more links
    return order is not PlayOrder.LEAD  # a Followup or Finish extends the chain


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
}
