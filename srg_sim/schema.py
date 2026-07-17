"""JSON Schema for the two pinned contracts: the Effect IR (§3) and the game log
(§8) — the language-neutral form both the Python oracle and the Rust engine
validate against (``docs/design/substrate-split.md`` §6/§9).

The schemas are **generated** from the frozen dataclasses (:mod:`srg_sim.effects`,
:mod:`srg_sim.gamelog`) rather than hand-authored, so they cannot silently drift
from the code. The generated documents are committed under ``schemas/v1/`` and a
conformance test (:mod:`tests.test_schema`) asserts three things: the committed
files match a fresh generation (drift guard), the schemas are themselves valid
JSON Schema, and real serialized IR / log output validates against them. A change
to the IR or the log therefore *fails* that test until the committed schema is
regenerated and its ``SCHEMA_VERSION`` deliberately bumped — exactly the
expensive-to-change review gate CLAUDE.md pins on §3/§8.

Encoding mirrors the two codecs verbatim: an IR node is a JSON object tagged by
``@type`` (its class name, :data:`srg_sim.effects._TAG`); a log event is tagged by
``type`` (its ``TYPE`` string). Enums serialize to their string ``value``. Every
serialized field is always present, so the schemas require all of them and forbid
extras (``additionalProperties: false``) — the strictest reading of a frozen
contract.
"""

from __future__ import annotations

import json
import types
from dataclasses import fields, is_dataclass
from enum import Enum
from pathlib import Path
from typing import Any, Union, get_args, get_origin, get_type_hints

from srg_sim import effects as fx
from srg_sim import gamelog as gl

# Bump deliberately when the IR (§3) or the log (§8) contract changes; the
# committed schema files live under ``schemas/v{SCHEMA_VERSION}/``.
SCHEMA_VERSION = 1

_DIALECT = "https://json-schema.org/draft/2020-12/schema"
_SCHEMAS_DIR = Path(__file__).resolve().parent.parent / "schemas" / f"v{SCHEMA_VERSION}"

_SCALARS: dict[type, dict[str, Any]] = {
    bool: {"type": "boolean"},  # before int: bool is an int subclass
    int: {"type": "integer"},
    float: {"type": "number"},
    str: {"type": "string"},
}


# ---------------------------------------------------------------------------
# Type-hint -> JSON Schema fragment
# ---------------------------------------------------------------------------


def _map(hint: Any, defs: dict[str, Any]) -> dict[str, Any]:
    """Map a resolved type hint to a JSON Schema fragment, registering any enum
    or dataclass it references into ``defs`` and referring to it by ``$ref``."""
    if hint is Any:
        return {}  # arbitrary JSON (Decision.legal/chosen, EffectApplied.detail)
    origin = get_origin(hint)
    if origin is None:
        return _map_atom(hint, defs)
    if origin in (list, tuple):
        return {"type": "array", "items": _map(get_args(hint)[0], defs)}
    if origin is dict:
        return {"type": "object", "additionalProperties": _map(get_args(hint)[1], defs)}
    if origin is Union or origin is types.UnionType:
        return _map_union(get_args(hint), defs)
    raise TypeError(f"unmapped type origin: {hint!r}")


def _map_atom(hint: type, defs: dict[str, Any]) -> dict[str, Any]:
    if hint in _SCALARS:
        return dict(_SCALARS[hint])
    if hint is type(None):
        return {"type": "null"}
    if isinstance(hint, type) and issubclass(hint, Enum):
        return _ref(_enum_def(hint, defs))
    if isinstance(hint, type) and is_dataclass(hint):
        return _ref(_record_def(hint, defs))
    raise TypeError(f"unmapped atom: {hint!r}")


def _map_union(args: tuple[Any, ...], defs: dict[str, Any]) -> dict[str, Any]:
    members = [_map(a, defs) for a in args if a is not type(None)]
    schema = members[0] if len(members) == 1 else {"oneOf": members}
    if type(None) in args:
        return {"anyOf": [schema, {"type": "null"}]}
    return schema


def _ref(name: str) -> dict[str, str]:
    return {"$ref": f"#/$defs/{name}"}


# ---------------------------------------------------------------------------
# $defs builders (enums, records/nodes)
# ---------------------------------------------------------------------------


def _enum_def(cls: type[Enum], defs: dict[str, Any]) -> str:
    name = cls.__name__
    if name not in defs:
        defs[name] = {"type": "string", "enum": [m.value for m in cls]}
    return name


def _record_def(cls: type, defs: dict[str, Any]) -> str:
    """Register (once) the object schema for a dataclass node and return its def
    name. ``tag`` is the discriminator: ``@type`` for IR nodes, ``type`` for log
    events, or ``None`` for the tagless helper records (Header, RollMod, ...)."""
    name = cls.__name__
    if name in defs:
        return name
    defs[name] = {}  # reserve to break recursion (Effect -> actions -> ... -> Effect)
    props: dict[str, Any] = {}
    required: list[str] = []
    tag_key, tag_val = _tag_of(cls)
    if tag_key:
        props[tag_key] = {"const": tag_val}
        required.append(tag_key)
    hints = get_type_hints(cls)
    aliases = getattr(cls, "_ALIASES", {})
    for f in fields(cls):
        key = aliases.get(f.name, f.name)
        props[key] = _map(hints[f.name], defs)
        required.append(key)
    defs[name] = {
        "type": "object",
        "properties": props,
        "required": required,
        "additionalProperties": False,
    }
    return name


def _tag_of(cls: type) -> tuple[str | None, str | None]:
    """The (discriminator-key, value) a serialized ``cls`` carries, if any."""
    if issubclass(cls, fx.IRNode):
        return fx._TAG, cls.__name__
    if issubclass(cls, gl.Event):
        return "type", cls.TYPE
    return None, None


# ---------------------------------------------------------------------------
# Public builders
# ---------------------------------------------------------------------------


def _document(title: str, root: dict[str, Any], defs: dict[str, Any]) -> dict[str, Any]:
    return {
        "$schema": _DIALECT,
        "$id": f"https://srg-sim/schemas/v{SCHEMA_VERSION}/{title}",
        "title": title,
        "version": SCHEMA_VERSION,
        **root,
        "$defs": defs,
    }


def build_ir_schema() -> dict[str, Any]:
    """Schema for any serialized Effect IR node (§3). Root = the IRNode union."""
    defs: dict[str, Any] = {}
    node_names = sorted(fx._REGISTRY)
    for name in node_names:
        _record_def(fx._REGISTRY[name], defs)
    defs["IRNode"] = {"oneOf": [_ref(n) for n in node_names]}
    return _document("effect_ir", {"$ref": "#/$defs/IRNode"}, defs)


def build_gamelog_schema() -> dict[str, Any]:
    """Schema for one JSON-Lines record of a game log (§8): the header, or any
    event. Root = ``oneOf`` [Header, Event-union]."""
    defs: dict[str, Any] = {}
    _record_def(gl.Header, defs)
    events = sorted(_concrete_events(), key=lambda c: c.TYPE)
    for cls in events:
        _record_def(cls, defs)
    defs["Event"] = {"oneOf": [_ref(c.__name__) for c in events]}
    root = {"oneOf": [_ref("Header"), _ref("Event")]}
    return _document("gamelog", root, defs)


def _concrete_events() -> list[type[gl.Event]]:
    """Every registered, non-abstract log event (excludes ``_CardMovement`` /
    ``Event`` whose ``TYPE`` is empty)."""
    return list(gl._EVENT_REGISTRY.values())


# ---------------------------------------------------------------------------
# On-disk artifacts
# ---------------------------------------------------------------------------

_BUILDERS = {"effect_ir": build_ir_schema, "gamelog": build_gamelog_schema}


def schema_path(name: str) -> Path:
    return _SCHEMAS_DIR / f"{name}.schema.json"


def load_schema(name: str) -> dict[str, Any]:
    return json.loads(schema_path(name).read_text())


def write_schemas() -> list[Path]:
    """(Re)generate and write both committed schema files. Returns the paths."""
    _SCHEMAS_DIR.mkdir(parents=True, exist_ok=True)
    written = []
    for name, build in _BUILDERS.items():
        path = schema_path(name)
        text = json.dumps(build(), indent=2, sort_keys=True) + "\n"
        path.write_text(text)
        written.append(path)
    return written


if __name__ == "__main__":  # regenerate the pinned artifacts
    for p in write_schemas():
        print(f"wrote {p}")
