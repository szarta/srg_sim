"""Tests for the srg-sim CLI (DESIGN.md §9): play, coverage, replay.

Runs fully offline against a synthetic ``cards.yaml`` fixture (via ``--cards``),
so no card DB is required.
"""

from __future__ import annotations

import csv
import json
from pathlib import Path
from typing import Any

import pytest
import yaml
from srg_sim.cli import main

_SKILLS = ("Power", "Technique", "Agility", "Submission", "Grapple", "Strike")


def _atk(n: int) -> str:
    return {0: "Submission", 1: "Strike", 2: "Grapple"}[n % 3]


def _order(n: int) -> str:
    return "Finish" if n >= 28 else ("Lead" if n <= 12 else "Followup")


def _main_rec(n: int) -> dict[str, Any]:
    text = f"+2 to {_atk(n)}" if n >= 28 else "+1 to Power"
    if n == 1:
        # A grammar hit plus an unsupported clause (for coverage variety).
        text = "+1 to Power\nSummon a dragon from the void."
    return {
        "card_type": "MainDeckCard",
        "db_uuid": f"m{n:02d}",
        "name": f"M{n:02d}",
        "deck_card_number": n,
        "atk_type": _atk(n),
        "play_order": _order(n),
        "rules_text": text,
        "tags": [],
    }


def _competitor(uuid: str, name: str, division: str, strike: int) -> dict[str, Any]:
    return {
        "card_type": "SingleCompetitorCard",
        "db_uuid": uuid,
        "name": name,
        "division": division,
        "power": 10,
        "technique": 6,
        "agility": 5,
        "submission": 8,
        "grapple": 9,
        "strike": strike,
        "rules_text": "When the moon is full, do something arcane.",  # unsupported gimmick
    }


def _entrance(uuid: str, name: str) -> dict[str, Any]:
    return {"card_type": "EntranceCard", "db_uuid": uuid, "name": name, "rules_text": ""}


def _decklist(competitor: str, entrance: str) -> dict[str, Any]:
    return {
        "competitor": competitor,
        "entrance": entrance,
        "cards": [{"number": n, "name": f"M{n:02d}"} for n in range(1, 31)],
    }


@pytest.fixture
def world(tmp_path: Path) -> dict[str, Path]:
    """A synthetic card export + two decklists, written to disk."""
    records: list[dict[str, Any]] = [_main_rec(n) for n in range(1, 31)]
    records += [
        _competitor("cA", "Comp A", "World Championship", 7),
        _competitor("cB", "Comp B", "Hardcore", 8),
        _entrance("eA", "Ent A"),
        _entrance("eB", "Ent B"),
    ]
    cards = tmp_path / "cards.yaml"
    cards.write_text(yaml.safe_dump(records))
    deck_a = tmp_path / "a.yaml"
    deck_a.write_text(yaml.safe_dump(_decklist("Comp A", "Ent A")))
    deck_b = tmp_path / "b.yaml"
    deck_b.write_text(yaml.safe_dump(_decklist("Comp B", "Ent B")))
    return {"cards": cards, "a": deck_a, "b": deck_b}


def _play_args(world: dict[str, Path], **extra: str) -> list[str]:
    args = ["play", str(world["a"]), str(world["b"]), "--cards", str(world["cards"])]
    for key, value in extra.items():
        args += [f"--{key.replace('_', '-')}", value]
    return args


# --- play -------------------------------------------------------------------


def test_play_prints_result(world: dict[str, Path], capsys: pytest.CaptureFixture[str]) -> None:
    assert main(_play_args(world, seed="7")) == 0
    out = capsys.readouterr().out
    assert "Comp A" in out and "Comp B" in out
    assert "result:" in out and "wins by" in out


def test_play_writes_a_parseable_log(
    world: dict[str, Path], tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    log = tmp_path / "game.jsonl"
    main(_play_args(world, seed="3", out=str(log)))
    from srg_sim.gamelog import GameLog

    parsed = GameLog.read(log)
    assert parsed.header.kind == "sim"
    assert parsed.events
    assert json.loads(log.read_text().splitlines()[-1])["type"] == "result"


def test_play_is_deterministic(world: dict[str, Path], capsys: pytest.CaptureFixture[str]) -> None:
    main(_play_args(world, seed="42"))
    first = capsys.readouterr().out
    main(_play_args(world, seed="42"))
    assert capsys.readouterr().out == first


def test_unknown_policy_exits(world: dict[str, Path]) -> None:
    with pytest.raises(SystemExit, match="unknown policy"):
        main(_play_args(world, policy_a="wizard"))


@pytest.mark.parametrize("profile", ["aggressive", "smart", "newbie"])
def test_player_profiles_are_selectable_and_play(
    world: dict[str, Path], profile: str, capsys: pytest.CaptureFixture[str]
) -> None:
    # Each player-profile policy (todo #32) is registered and runs a full match.
    assert main(_play_args(world, seed="5", policy_a=profile, policy_b=profile)) == 0
    assert "result:" in capsys.readouterr().out


def test_missing_cards_file_exits(world: dict[str, Path]) -> None:
    with pytest.raises(SystemExit, match="card export not found"):
        main(["coverage", "--cards", "/no/such/cards.yaml"])


# --- analyze ----------------------------------------------------------------


def _analyze_args(world: dict[str, Path], **extra: str) -> list[str]:
    args = ["analyze", str(world["a"]), str(world["b"]), "--cards", str(world["cards"])]
    for key, value in extra.items():
        args += [f"--{key.replace('_', '-')}", value]
    return args


def test_analyze_prints_report_summary(
    world: dict[str, Path], capsys: pytest.CaptureFixture[str]
) -> None:
    assert main(_analyze_args(world, games="6", seed_start="0")) == 0
    out = capsys.readouterr().out
    assert "analyze:" in out and "6 games" in out and "seeds 0-5" in out
    assert "wins:" in out and "reasons:" in out
    assert "length (turns):" in out and "stops/game:" in out


def test_analyze_respects_seed_start(
    world: dict[str, Path], capsys: pytest.CaptureFixture[str]
) -> None:
    main(_analyze_args(world, games="4", seed_start="10"))
    assert "seeds 10-13" in capsys.readouterr().out


def test_analyze_writes_json(
    world: dict[str, Path], tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    out = tmp_path / "report.json"
    main(_analyze_args(world, games="5", json=str(out)))
    blob = json.loads(out.read_text())
    assert blob["games"] == 5
    assert blob["wins"]["A"] + blob["wins"]["B"] + blob["wins"]["draw"] == 5
    assert isinstance(blob["win_ci"]["A"], list) and len(blob["win_ci"]["A"]) == 2


def test_analyze_writes_long_format_csv(
    world: dict[str, Path], tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    out = tmp_path / "report.csv"
    main(_analyze_args(world, games="5", csv=str(out)))
    rows = list(csv.reader(out.read_text().splitlines()))
    assert rows[0] == ["metric", "value"]
    flat = dict(rows[1:])
    assert flat["games"] == "5"
    assert "win_rate.A" in flat and "reasons.finish" in " ".join(flat)  # nested keys dot-joined


def test_analyze_is_deterministic(
    world: dict[str, Path], capsys: pytest.CaptureFixture[str]
) -> None:
    main(_analyze_args(world, games="8", seed_start="3"))
    first = capsys.readouterr().out
    main(_analyze_args(world, games="8", seed_start="3"))
    assert capsys.readouterr().out == first


def test_analyze_unknown_policy_exits(world: dict[str, Path]) -> None:
    with pytest.raises(SystemExit, match="unknown policy"):
        main(_analyze_args(world, games="2", policy_a="wizard"))


def test_bad_deck_ref_exits(world: dict[str, Path], tmp_path: Path) -> None:
    bad = tmp_path / "bad.yaml"
    bad.write_text(yaml.safe_dump({"competitor": "Nobody", "entrance": "Ent A", "cards": []}))
    with pytest.raises(SystemExit, match="could not load deck"):
        main(["play", str(bad), str(world["b"]), "--cards", str(world["cards"])])


# --- coverage ---------------------------------------------------------------


def test_coverage_reports_main_deck(
    world: dict[str, Path], capsys: pytest.CaptureFixture[str]
) -> None:
    assert main(["coverage", "--cards", str(world["cards"])]) == 0
    out = capsys.readouterr().out
    assert "main deck" in out
    assert "parsed" in out
    assert "unsupported" in out  # M01's "Summon a dragon" clause


def test_coverage_top96(world: dict[str, Path], capsys: pytest.CaptureFixture[str]) -> None:
    main(["coverage", "--top96", "--cards", str(world["cards"])])
    assert "top-96 competitors" in capsys.readouterr().out


# --- replay -----------------------------------------------------------------


def test_replay_reproduces(
    world: dict[str, Path], tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    log = tmp_path / "game.jsonl"
    main(_play_args(world, seed="9", out=str(log)))
    capsys.readouterr()  # clear
    assert main(["replay", str(log), "--cards", str(world["cards"])]) == 0
    assert "replay OK" in capsys.readouterr().out


def test_replay_detects_tampering(
    world: dict[str, Path], tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    log = tmp_path / "game.jsonl"
    main(_play_args(world, seed="9", out=str(log)))
    lines = log.read_text().splitlines()
    # Corrupt the first event so the regenerated stream can't match it.
    event = json.loads(lines[1])
    event["player"] = "Z"
    lines[1] = json.dumps(event)
    log.write_text("\n".join(lines) + "\n")
    capsys.readouterr()
    assert main(["replay", str(log), "--cards", str(world["cards"])]) == 1
    assert "MISMATCH" in capsys.readouterr().out


def test_replay_rejects_non_sim_log(tmp_path: Path, world: dict[str, Path]) -> None:
    from srg_sim.gamelog import GameLog, Header, PlayerInfo, Result

    header = Header(
        seed=1,
        kind="real",
        created="x",
        players={
            "A": PlayerInfo("Comp A", "Ent A", ["m01"], "human"),
            "B": PlayerInfo("Comp B", "Ent B", ["m01"], "human"),
        },
    )
    log = tmp_path / "human.jsonl"
    GameLog(header, [Result(t=1, winner="A", reason="pinfall", turns=1)]).write(log)
    with pytest.raises(SystemExit, match="only sim logs"):
        main(["replay", str(log), "--cards", str(world["cards"])])
