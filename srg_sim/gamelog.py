"""Game-log event schema, JSONL read/write, replay/verify (DESIGN.md §8).

One schema serves **both** simulated and recorded-human games: a header line
plus an ordered stream of events, one JSON object per line. A recorded human
match is the same schema with ``policy: "human"``. The stream is enough to (a)
deterministically replay a sim, (b) transcribe a real match, and (c) train a
policy — ``decision`` events (legal set + chosen action) are the training signal.

Serialization is generic and dataclass-driven: each record's fields map to JSON
keys (with an optional alias, used for the reserved word ``from``); nested
records and tuples are rebuilt from field type hints on read. Skill / attack-type
/ order values are stored as their plain strings (``"Strike"``, ``"Lead"``) so a
human can read and transcribe a log without the internal enums.

Replay (DESIGN.md §8): re-run the engine from ``header.seed`` and assert the
produced stream matches the recorded one — :func:`diff` reports the first
divergences. The engine hookup lands with the engine itself; this module owns
the schema, the JSONL round-trip, and the stream comparison.
"""

from __future__ import annotations

import json
import types
from collections.abc import Iterable
from dataclasses import dataclass, field, fields
from pathlib import Path
from typing import Any, ClassVar, Union, get_args, get_origin, get_type_hints

SCHEMA_VERSION = 1

# ---------------------------------------------------------------------------
# Generic dataclass <-> dict codec
# ---------------------------------------------------------------------------


def _encode(value: Any) -> Any:
    if isinstance(value, _Record):
        return value.to_dict()
    if isinstance(value, list | tuple):
        return [_encode(v) for v in value]
    if isinstance(value, dict):
        return {k: _encode(v) for k, v in value.items()}
    return value


def _decode(value: Any, hint: Any) -> Any:
    if value is None:
        return None
    origin = get_origin(hint)
    if origin is None:
        if isinstance(hint, type) and issubclass(hint, _Record):
            return hint.from_dict(value)
        return value
    if origin in (list, tuple):
        elem = get_args(hint)[0]
        decoded = [_decode(v, elem) for v in value]
        return tuple(decoded) if origin is tuple else decoded
    if origin is dict:
        _, val_hint = get_args(hint)
        return {k: _decode(v, val_hint) for k, v in value.items()}
    if origin is Union or origin is types.UnionType:
        variants = [a for a in get_args(hint) if a is not type(None)]
        return _decode(value, variants[0]) if len(variants) == 1 else value
    return value


@dataclass(frozen=True)
class _Record:
    """Base for every serializable log record (header, events, sub-structs)."""

    # field name -> JSON key, for names that can't be Python identifiers.
    _ALIASES: ClassVar[dict[str, str]] = {}

    def to_dict(self) -> dict[str, Any]:
        out: dict[str, Any] = {}
        for f in fields(self):
            out[self._ALIASES.get(f.name, f.name)] = _encode(getattr(self, f.name))
        return out

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> _Record:
        hints = get_type_hints(cls)
        kwargs = {}
        for f in fields(cls):
            key = cls._ALIASES.get(f.name, f.name)
            if key in data:
                kwargs[f.name] = _decode(data[key], hints[f.name])
        return cls(**kwargs)


# ---------------------------------------------------------------------------
# Header and sub-structures
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class PlayerInfo(_Record):
    competitor: str
    entrance: str
    deck: list[str] = field(default_factory=list)  # card refs (db_uuid or name)
    policy: str = ""


@dataclass(frozen=True)
class Header(_Record):
    seed: int
    kind: str  # "sim" | "real"
    created: str  # caller-supplied timestamp (kept out of the engine for determinism)
    players: dict[str, PlayerInfo] = field(default_factory=dict)
    schema: int = SCHEMA_VERSION


@dataclass(frozen=True)
class RollMod(_Record):
    src: str
    delta: int


@dataclass(frozen=True)
class BreakoutRoll(_Record):
    skill: str
    value: int
    penalty: int
    success: bool


# ---------------------------------------------------------------------------
# Events
# ---------------------------------------------------------------------------

_EVENT_REGISTRY: dict[str, type[Event]] = {}


@dataclass(frozen=True)
class Event(_Record):
    """Base event. Every event carries the turn number ``t`` and a ``type`` tag.

    Subclasses set ``TYPE`` (the schema's ``type`` string) and are auto-registered
    for deserialization.
    """

    TYPE: ClassVar[str] = ""

    t: int

    def __init_subclass__(cls, **kwargs: Any) -> None:
        super().__init_subclass__(**kwargs)
        tag = cls.__dict__.get("TYPE")
        if tag:
            _EVENT_REGISTRY[tag] = cls

    def to_dict(self) -> dict[str, Any]:
        rest = super().to_dict()
        return {"t": rest.pop("t"), "type": self.TYPE, **rest}


@dataclass(frozen=True)
class Roll(Event):
    player: str
    skill: str
    base: int
    value: int
    mods: tuple[RollMod, ...] = ()
    TYPE: ClassVar[str] = "roll"


@dataclass(frozen=True)
class TurnResult(Event):
    winner: str
    tie_bumps: int = 0
    TYPE: ClassVar[str] = "turn_result"


@dataclass(frozen=True)
class Decision(Event):
    player: str
    point: str  # turn_action | stop | finish | mulligan | target | optional
    legal: list[Any] = field(default_factory=list)
    chosen: Any = None
    policy: str = ""
    TYPE: ClassVar[str] = "decision"


@dataclass(frozen=True)
class Play(Event):
    player: str
    card: str
    order: str
    atk_type: str
    TYPE: ClassVar[str] = "play"


@dataclass(frozen=True)
class Stop(Event):
    player: str
    card: str
    stopped: str
    reason: str = ""
    TYPE: ClassVar[str] = "stop"


@dataclass(frozen=True)
class _CardMovement(Event):
    """Shared shape for draw / bury / discard / search (``from`` is optional).

    ``hidden`` marks a move the opponent cannot follow card-for-card (DESIGN.md
    §7/§8): true iff BOTH endpoints are private zones — hand or deck — so the
    opponent sees only that *some* card(s) moved, not which. A move touching any
    public zone (discard, in_play, competitor board) is diffable and stays false.
    In practice only a deck→hand draw and a hand→deck bury are hidden; every
    discard/mill/recycle has a public endpoint. The ground-truth card ids stay in
    the log for deterministic replay; ``hidden`` gates what an observer projection
    (:meth:`GameState.observable`) is allowed to reveal.
    """

    _ALIASES: ClassVar[dict[str, str]] = {"source": "from"}

    player: str
    cards: list[str] = field(default_factory=list)
    source: str | None = None  # serialized as "from" (e.g. TOP | BOTTOM)
    hidden: bool = False  # opponent cannot identify the moved cards (both ends private)


@dataclass(frozen=True)
class Draw(_CardMovement):
    TYPE: ClassVar[str] = "draw"


@dataclass(frozen=True)
class Bury(_CardMovement):
    TYPE: ClassVar[str] = "bury"


@dataclass(frozen=True)
class Discard(_CardMovement):
    TYPE: ClassVar[str] = "discard"


@dataclass(frozen=True)
class Search(_CardMovement):
    TYPE: ClassVar[str] = "search"


@dataclass(frozen=True)
class FinishAttempt(Event):
    player: str
    finish: str
    value: int
    crowd_meter: int
    auto_success: bool
    bonus: dict[str, int] = field(default_factory=dict)
    TYPE: ClassVar[str] = "finish_attempt"


@dataclass(frozen=True)
class Breakout(Event):
    defender: str
    broke_out: bool
    rolls: tuple[BreakoutRoll, ...] = ()
    TYPE: ClassVar[str] = "breakout"


@dataclass(frozen=True)
class CrowdMeter(Event):
    delta: int
    value: int
    TYPE: ClassVar[str] = "crowd_meter"


@dataclass(frozen=True)
class Unsupported(Event):
    owner: str
    raw: str
    reason: str
    card: str | None = None
    gimmick: str | None = None
    TYPE: ClassVar[str] = "unsupported"


@dataclass(frozen=True)
class EffectApplied(Event):
    src: str
    action: str
    target: str | None = None
    detail: Any = None
    TYPE: ClassVar[str] = "effect"


@dataclass(frozen=True)
class Result(Event):
    winner: str
    reason: str  # finish | count_out | disqualification | pinfall
    turns: int
    TYPE: ClassVar[str] = "result"


def event_from_dict(data: dict[str, Any]) -> Event:
    """Rebuild an event from its serialized form, dispatching on ``type``."""
    tag = data["type"]
    if tag not in _EVENT_REGISTRY:
        raise ValueError(f"unknown event type: {tag!r}")
    result = _EVENT_REGISTRY[tag].from_dict(data)
    assert isinstance(result, Event)
    return result


# ---------------------------------------------------------------------------
# Log container: JSONL read/write + verification
# ---------------------------------------------------------------------------


@dataclass
class GameLog:
    """A header plus an ordered list of events. Mutable so the engine can append
    events as a game plays out.
    """

    header: Header
    events: list[Event] = field(default_factory=list)

    def append(self, event: Event) -> None:
        self.events.append(event)

    def to_lines(self) -> list[str]:
        """Serialize to JSONL: the header line followed by one line per event."""
        lines = [json.dumps(self.header.to_dict())]
        lines.extend(json.dumps(e.to_dict()) for e in self.events)
        return lines

    def write(self, path: str | Path) -> None:
        Path(path).write_text("\n".join(self.to_lines()) + "\n")

    @classmethod
    def parse(cls, lines: Iterable[str]) -> GameLog:
        rows = [line for line in lines if line.strip()]
        if not rows:
            raise ValueError("empty log: no header line")
        header = Header.from_dict(json.loads(rows[0]))
        assert isinstance(header, Header)
        events = [event_from_dict(json.loads(row)) for row in rows[1:]]
        return cls(header=header, events=events)

    @classmethod
    def read(cls, path: str | Path) -> GameLog:
        return cls.parse(Path(path).read_text().splitlines())


def diff(expected: GameLog, actual: GameLog) -> list[str]:
    """Structural differences between two logs (empty means they match).

    This is the core of replay verification: re-run the engine from the header
    seed, then ``diff`` the produced log against the recorded one.
    """
    problems: list[str] = []
    if expected.header != actual.header:
        problems.append("header mismatch")
    if len(expected.events) != len(actual.events):
        problems.append(f"event count: expected {len(expected.events)}, got {len(actual.events)}")
    for i, (exp, act) in enumerate(zip(expected.events, actual.events, strict=False)):
        if exp != act:
            problems.append(f"event {i} differs: {exp!r} != {act!r}")
    return problems


def matches(expected: GameLog, actual: GameLog) -> bool:
    """True iff two logs are structurally identical."""
    return not diff(expected, actual)
