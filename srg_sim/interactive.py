"""Interactive human play: a terminal :class:`Policy` over the observable view (§7).

The instrumentation behind todo #42 — a person plays one side of a real match at
the terminal while the engine (assisted, for now, by a strong policy) plays the
other. The human is shown **only** what a player at the table may know
(:meth:`GameState.observable`: their own hand, the opponent's hand *size*, deck
*sizes*) and picks from the same ``legal`` option set every policy sees, so the
choice is captured as an ordinary ``decision`` event — no schema change, and the
match replays and reviews like any other (see :mod:`srg_sim.review`).

Rendering and input are the only I/O in the package; both are injected
(``out`` / ``ask``) so the loop is unit-testable with fakes and never couples the
engine to a real terminal. Critique is deliberately **not** offered here: the
human's decisions must be captured unassisted (todo #42, decision 2 — clean
signal), so all coaching happens post-game against the reconstructed oracle view.
"""

from __future__ import annotations

from collections.abc import Callable
from typing import TYPE_CHECKING

from srg_sim.policy import Option, Policy

if TYPE_CHECKING:
    from srg_sim.state import GameState

Printer = Callable[[str], None]
Asker = Callable[[str], str]


class HumanPolicy(Policy):
    """Prompt a human for each decision, showing only the observable view (§7).

    ``out`` receives rendered lines (defaults to :func:`print`); ``ask`` returns
    one line of input for a prompt (defaults to :func:`input`). Injecting both
    keeps the class free of hard terminal coupling and lets tests drive a scripted
    session. Invalid input re-prompts rather than raising — a person is at the
    keyboard.
    """

    def __init__(
        self, name: str = "human", out: Printer | None = None, ask: Asker | None = None
    ) -> None:
        super().__init__(name)
        self._out = out or print
        self._ask = ask or input

    def choose(self, point: str, legal: list[Option], state: GameState, key: str) -> Option:
        for line in render_view(state, key):
            self._out(line)
        self._out("")
        for line in render_options(point, legal):
            self._out(line)
        return legal[self._prompt_index(len(legal))]

    def _prompt_index(self, count: int) -> int:
        """Read a 1-based option number, re-prompting until it is in range."""
        while True:
            raw = self._ask(f"choose [1-{count}]: ").strip()
            if raw.isdigit() and 1 <= int(raw) <= count:
                return int(raw) - 1
            self._out(f"  please enter a number from 1 to {count}")


# -- rendering (pure: state/options -> lines) --------------------------------


def render_view(state: GameState, viewer: str) -> list[str]:
    """The observable position as ``viewer`` sees it, as printable lines (§7).

    Built from :meth:`GameState.observable` only, so it can never leak hidden
    state: the opponent's hand and every deck show as counts, the viewer's own
    hand shows in full.
    """
    view = state.observable(viewer)
    opp = state.opponent_of(viewer)
    lines = [
        f"── turn {view['turn_no']}  ·  crowd meter {view['crowd_meter']}  ·  you are {viewer} ──"
    ]
    lines += _opponent_lines(view["players"][opp], opp)
    lines += _self_lines(view["players"][viewer], viewer)
    return lines


def _opponent_lines(pv: dict, key: str) -> list[str]:
    return [
        f"opponent {key}: {pv['competitor']['name']}",
        f"  in play: {_cards(pv['in_play']) or '(empty)'}",
        f"  hand: {pv['hand_size']} cards   deck: {pv['deck_size']}   "
        f"discard: {len(pv['discard'])}",
    ]


def _self_lines(pv: dict, key: str) -> list[str]:
    return [
        f"you {key}: {pv['competitor']['name']}",
        f"  in play: {_cards(pv['in_play']) or '(empty)'}",
        f"  deck: {pv['deck_size']}   discard: {len(pv['discard'])}",
        f"  hand: {_cards(pv['hand']) or '(empty)'}",
    ]


def _cards(cards: list[dict]) -> str:
    """A compact ``#num name (Order/Type)`` listing for a zone."""
    return ", ".join(_card_label(c) for c in cards)


def _card_label(card: dict) -> str:
    tags = "/".join(t for t in (card.get("play_order"), card.get("atk_type")) if t)
    suffix = f" ({tags})" if tags else ""
    return f"#{card['number']} {card['name']}{suffix}"


def render_options(point: str, legal: list[Option]) -> list[str]:
    """A numbered menu of the legal options for ``point`` (1-based)."""
    lines = [f"decision: {point}"]
    lines += [f"  {i + 1}) {_option_label(o)}" for i, o in enumerate(legal)]
    return lines


def _option_label(option: Option) -> str:
    kind = option.get("kind", "?")
    if kind == "none":
        return f"do not stop (vs {option.get('vs_order')} {option.get('vs_type')})"
    if kind in ("play", "stop", "discard", "bury"):
        return f"{kind} " + _move_label(option)
    return kind


def _move_label(option: Option) -> str:
    parts = [f"#{option['number']}"] if "number" in option else []
    if "card" in option and "number" not in option:
        parts.append(str(option["card"]))
    tags = "/".join(str(option[k]) for k in ("order", "atk_type") if k in option)
    if tags:
        parts.append(f"({tags})")
    return " ".join(parts)
