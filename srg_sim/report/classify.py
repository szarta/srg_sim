"""Hybrid competitor-type classifier: heuristic over compiled IR + YAML override.

A competitor's "type" (draw / turn-advantage / recursion / discard / keyword /
control / stat-based) is inferred from its compiled gimmick effects — the same IR
the engine runs — by matching action/trigger shapes to labels in priority order. A
hand-authored override (``report/overrides.yaml``, keyed by name or uuid) wins when
present, so a nuanced gimmick can be corrected without touching code.
"""

from __future__ import annotations

from collections.abc import Iterable
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import yaml

from srg_sim import effects as fx
from srg_sim.cards import Competitor

OVERRIDES_YAML = Path(__file__).resolve().parent / "overrides.yaml"


@dataclass(frozen=True)
class CompType:
    """A competitor's inferred type: primary ``label`` + any secondary matches."""

    label: str
    also: tuple[str, ...]
    signals: tuple[str, ...]  # the raw clauses that fired, for transparency
    source: str  # "override" | "heuristic"


# Priority-ordered signal -> label rules (first match is the primary label).
def _is_turn(eff: fx.Effect) -> bool:
    turn_actions = (fx.ModifyRoll, fx.LowestRollWins, fx.WinTie, fx.Bump, fx.Reroll)
    return isinstance(eff.trigger, (fx.OnRoll, fx.OnBump)) or any(
        isinstance(a, turn_actions) for a in eff.actions
    )


_RECUR = (fx.AddFromDiscard, fx.ShuffleIntoDeck, fx.RecurToDeckTop)


def _is_recursion(eff: fx.Effect) -> bool:
    return any(isinstance(a, _RECUR) for a in eff.actions)


def _is_discard(eff: fx.Effect) -> bool:
    return any(
        isinstance(a, (fx.Discard, fx.Bury)) and a.who is fx.Who.OPP for a in eff.actions
    )


def _is_keyword(eff: fx.Effect) -> bool:
    # A keyword/synergy gimmick: an OnStop, or an OnHit that keys off a specific
    # keyword/name (a bare OnHit is just "when this resolves" — e.g. Draw — not keyword).
    trig = eff.trigger
    if isinstance(trig, fx.OnStop):
        return True
    return isinstance(trig, fx.OnHit) and (trig.keyword is not None or trig.name is not None)


def _is_control(eff: fx.Effect) -> bool:
    return any(isinstance(a, (fx.BlankGimmick, fx.BlankText, fx.Stop)) for a in eff.actions)


def _is_draw(eff: fx.Effect) -> bool:
    return any(isinstance(a, fx.Draw) and a.who is fx.Who.SELF for a in eff.actions)


_RULES: tuple[tuple[str, Any], ...] = (
    ("turn-advantage", _is_turn),
    ("recursion", _is_recursion),
    ("discard", _is_discard),
    ("keyword", _is_keyword),
    ("control", _is_control),
    ("draw", _is_draw),
)

VANILLA = "stat-based"


def load_overrides(path: str | Path = OVERRIDES_YAML) -> dict[str, Any]:
    """Load the curation overrides (``{name_or_uuid: {type, also, notes, ...}}``)."""
    if not Path(path).exists():
        return {}
    raw = yaml.safe_load(Path(path).read_text())
    return raw or {}


def classify(comp: Competitor, overrides: dict[str, Any] | None = None) -> CompType:
    """Infer ``comp``'s type: an override entry wins, else heuristics over its IR."""
    entry = _override_entry(comp, overrides or {})
    if entry and entry.get("type"):
        return CompType(
            label=str(entry["type"]),
            also=tuple(entry.get("also") or ()),
            signals=(),
            source="override",
        )
    return _heuristic(comp.effects)


def _override_entry(comp: Competitor, overrides: dict[str, Any]) -> dict[str, Any] | None:
    for key in (comp.db_uuid, comp.name):
        if key in overrides:
            return overrides[key]
    return None


def _heuristic(effects: Iterable[fx.Effect]) -> CompType:
    effects = list(effects)
    matched: list[str] = []
    signals: list[str] = []
    for label, predicate in _RULES:
        firing = [eff for eff in effects if predicate(eff)]
        if firing:
            matched.append(label)
            signals.extend(eff.raw_clause for eff in firing if eff.raw_clause)
    if not matched:
        return CompType(VANILLA, (), (), "heuristic")
    return CompType(matched[0], tuple(matched[1:]), tuple(dict.fromkeys(signals)), "heuristic")
