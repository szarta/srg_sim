"""Tests for the game-log schema: JSONL round-trip, coverage, verify (§8)."""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from srg_sim.gamelog import (
    _EVENT_REGISTRY,
    SCHEMA_VERSION,
    Breakout,
    BreakoutRoll,
    Bury,
    CrowdMeter,
    Decision,
    Discard,
    Draw,
    EffectApplied,
    Event,
    FinishAttempt,
    GameLog,
    Header,
    Play,
    PlayerInfo,
    Result,
    Roll,
    RollMod,
    Search,
    Stop,
    TurnResult,
    Unsupported,
    diff,
    event_from_dict,
    matches,
)


def _header() -> Header:
    return Header(
        seed=11,
        kind="sim",
        created="2026-07-14T00:00:00Z",
        players={
            "A": PlayerInfo("The Bull", "Calling in Kanik", ["u1", "u2"], "heuristic"),
            "B": PlayerInfo("Fae Dragon", "Grand Entrance", ["u3", "u4"], "random"),
        },
    )


# One instance of every registered event type; the coverage test keeps this in
# lock-step with the registry.
def _all_events() -> list[Event]:
    return [
        Roll(t=1, player="A", skill="Strike", base=7, value=8, mods=(RollMod("gimmick", 1),)),
        TurnResult(t=1, winner="A", tie_bumps=2),
        Decision(
            t=1, player="A", point="turn_action", legal=[0, 1, 2], chosen=1, policy="heuristic"
        ),
        Play(t=1, player="A", card="u-lead", order="Lead", atk_type="Strike"),
        Stop(t=1, player="B", card="u-stop", stopped="u-lead", reason="Strike stops Grapple"),
        Draw(t=1, player="A", cards=["u-x"], source="TOP"),
        Bury(t=1, player="A", cards=["u-y"]),
        Discard(t=2, player="B", cards=["u-z"]),
        Search(t=2, player="A", cards=["u-w"], source="deck"),
        FinishAttempt(
            t=3,
            player="A",
            finish="u-28",
            value=11,
            crowd_meter=1,
            auto_success=True,
            bonus={"Strike": 1},
        ),
        Breakout(t=3, defender="B", broke_out=False, rolls=(BreakoutRoll("Power", 10, 0, False),)),
        CrowdMeter(t=3, delta=1, value=2),
        Unsupported(t=2, owner="A", raw="weird clause", reason="no grammar", card="u-7"),
        EffectApplied(
            t=2, src="u-lead", action="BuffSkill", target="A", detail={"skill": "Strike"}
        ),
        Result(t=3, winner="A", reason="finish", turns=3),
    ]


def _sample_log() -> GameLog:
    return GameLog(header=_header(), events=_all_events())


def test_sample_covers_every_registered_event() -> None:
    covered = {type(e).TYPE for e in _all_events()}
    assert covered == set(_EVENT_REGISTRY), (
        f"missing={set(_EVENT_REGISTRY) - covered} extra={covered - set(_EVENT_REGISTRY)}"
    )


@pytest.mark.parametrize("event", _all_events(), ids=lambda e: type(e).TYPE)
def test_event_round_trip(event: Event) -> None:
    assert event_from_dict(json.loads(json.dumps(event.to_dict()))) == event


def test_full_log_round_trip_through_lines() -> None:
    log = _sample_log()
    assert GameLog.parse(log.to_lines()) == log


def test_log_round_trip_through_file(tmp_path: Path) -> None:
    log = _sample_log()
    path = tmp_path / "game.jsonl"
    log.write(path)
    assert GameLog.read(path) == log


def test_header_line_shape() -> None:
    header_dict = json.loads(_sample_log().to_lines()[0])
    assert header_dict["schema"] == SCHEMA_VERSION
    assert header_dict["seed"] == 11
    assert header_dict["kind"] == "sim"
    assert "type" not in header_dict  # the header is not an event
    assert header_dict["players"]["A"]["policy"] == "heuristic"


def test_event_line_has_turn_and_type() -> None:
    d = Roll(t=4, player="A", skill="Power", base=10, value=10).to_dict()
    assert d["t"] == 4
    assert d["type"] == "roll"
    assert list(d)[:2] == ["t", "type"]  # t and type lead the object


def test_from_alias_is_used_for_card_movement() -> None:
    d = Draw(t=1, player="A", cards=["u-x"], source="BOTTOM").to_dict()
    assert d["from"] == "BOTTOM"  # 'source' serializes to the reserved word 'from'
    assert "source" not in d
    assert event_from_dict(d) == Draw(t=1, player="A", cards=["u-x"], source="BOTTOM")


def test_optional_from_omitted_none_survives() -> None:
    bury = Bury(t=1, player="A", cards=["u-y"])
    assert bury.to_dict()["from"] is None
    assert event_from_dict(bury.to_dict()) == bury


def test_real_game_uses_human_policy() -> None:
    header = Header(
        seed=0,
        kind="real",
        created="2026-07-14",
        players={
            "A": PlayerInfo("Comp A", "Ent A", ["u1"], "human"),
            "B": PlayerInfo("Comp B", "Ent B", ["u2"], "human"),
        },
    )
    log = GameLog(header, [Result(t=12, winner="A", reason="pinfall", turns=12)])
    assert GameLog.parse(log.to_lines()) == log


def test_event_from_dict_unknown_type_raises() -> None:
    with pytest.raises(ValueError, match="unknown event type"):
        event_from_dict({"t": 1, "type": "no_such_event"})


def test_card_movement_hidden_defaults_false_and_is_omittable() -> None:
    # A public move (discard) leaves hidden at its default; old logs without the
    # key still parse (§8 backward-compatible field).
    disc = Discard(t=3, player="A", cards=["m01"])
    assert disc.hidden is False
    assert event_from_dict({"t": 3, "type": "discard", "player": "A", "cards": ["m01"]}) == disc


def test_card_movement_hidden_round_trips() -> None:
    # A private->private move (deck->hand draw) carries hidden=True through JSONL.
    draw = Draw(t=5, player="B", cards=["m07"], source="TOP", hidden=True)
    restored = event_from_dict(json.loads(json.dumps(draw.to_dict())))
    assert restored == draw
    assert restored.hidden is True  # type: ignore[attr-defined]
    bury = Bury(t=6, player="A", cards=["m02"], source="hand", hidden=True)
    assert event_from_dict(json.loads(json.dumps(bury.to_dict()))) == bury


# --- verification (replay support) -----------------------------------------


def test_matching_logs_have_no_diff() -> None:
    assert matches(_sample_log(), _sample_log())
    assert diff(_sample_log(), _sample_log()) == []


def test_diff_reports_header_and_event_divergence() -> None:
    expected = _sample_log()
    actual = _sample_log()
    actual.header = Header(seed=99, kind="sim", created="x", players=expected.header.players)
    actual.events[0] = Roll(t=1, player="A", skill="Grapple", base=9, value=9)
    problems = diff(expected, actual)
    assert any("header mismatch" in p for p in problems)
    assert any("event 0 differs" in p for p in problems)


def test_diff_reports_event_count() -> None:
    expected = _sample_log()
    actual = GameLog(expected.header, expected.events[:-1])
    assert any("event count" in p for p in diff(expected, actual))


def test_empty_log_parse_raises() -> None:
    with pytest.raises(ValueError, match="empty log"):
        GameLog.parse([])
