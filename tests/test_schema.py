"""Conformance tests for the pinned §3/§8 JSON Schemas (:mod:`srg_sim.schema`).

These guard the two expensive-to-change contracts (DESIGN.md §3 Effect IR, §8 game
log). If the IR or the log gains/loses/retypes a field, :func:`test_committed_*`
fails until the committed ``schemas/v1/*.schema.json`` is regenerated
(``python -m srg_sim.schema``) and ``schema.SCHEMA_VERSION`` bumped — the deliberate
review gate. The other tests assert the schemas are valid JSON Schema and that
real serialized output actually validates against them (and that malformed output
does not, so the contract genuinely constrains).
"""

from __future__ import annotations

import pytest
from jsonschema import Draft202012Validator
from jsonschema.exceptions import ValidationError
from srg_sim import schema as S
from srg_sim.engine import Engine
from srg_sim.policy import RandomPolicy

from tests import demo_decks as dd

_NAMES = ("effect_ir", "gamelog")


@pytest.mark.parametrize("name", _NAMES)
def test_committed_schema_is_valid_and_matches_generator(name: str) -> None:
    """The committed file is valid JSON Schema and identical to a fresh build —
    the drift guard that fails on any un-regenerated IR/log change."""
    doc = S.load_schema(name)
    Draft202012Validator.check_schema(doc)
    assert doc == S._BUILDERS[name](), (
        f"schemas/v{S.SCHEMA_VERSION}/{name}.schema.json is stale — "
        "run `python -m srg_sim.schema` and bump SCHEMA_VERSION if §3/§8 changed"
    )


def _demo_effects() -> list:
    effects = []
    for make in (dd.bull_gimmick, dd.fae_gimmick):
        effects += list(make().effects)
    for deck in dd.bull_vs_fae():
        for card in deck.cards:
            effects += list(card.effects)
    return effects


def test_real_ir_conforms() -> None:
    """Every effect on the demo decks' cards + gimmicks validates — exercising
    nested triggers, conditions, action tuples, enums, and optionals."""
    validator = Draft202012Validator(S.load_schema("effect_ir"))
    effects = _demo_effects()
    assert effects, "expected demo decks to carry effects"
    for effect in effects:
        validator.validate(effect.to_dict())


def test_real_gamelog_conforms() -> None:
    """A full demo game's header and every event line validate against §8."""
    deck_a, deck_b = dd.bull_vs_fae()
    engine = Engine(deck_a, deck_b, RandomPolicy(), RandomPolicy(), seed=7)
    engine.play()
    log = engine.state.log
    assert log is not None
    validator = Draft202012Validator(S.load_schema("gamelog"))
    validator.validate(log.header.to_dict())
    events = log.events
    assert events, "expected the game to emit events"
    for event in events:
        validator.validate(event.to_dict())


def test_schema_rejects_malformed_ir() -> None:
    """The IR schema genuinely constrains: an unknown ``@type`` and a missing
    required field are both rejected."""
    validator = Draft202012Validator(S.load_schema("effect_ir"))
    with pytest.raises(ValidationError):
        validator.validate({"@type": "NotARealNode"})
    with pytest.raises(ValidationError):
        validator.validate({"@type": "Stop"})  # missing order/atk_type/source_is_skillreq


def test_schema_rejects_malformed_event() -> None:
    """The log schema rejects an unknown event ``type`` and extra properties."""
    validator = Draft202012Validator(S.load_schema("gamelog"))
    with pytest.raises(ValidationError):
        validator.validate({"t": 1, "type": "not_an_event"})
    with pytest.raises(ValidationError):
        validator.validate(
            {"t": 1, "type": "turn_result", "winner": "A", "tie_bumps": 0, "extra": 1}
        )
