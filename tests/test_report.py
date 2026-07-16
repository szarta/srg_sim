"""Tests for the matchup-report generator (srg_sim/report), fully offline.

Runs against a synthetic in-memory card set (no card DB, no images), mirroring the
``tests/test_cli.py`` fixture style. The Sphinx-build test is guarded on ``sphinx``
being importable; nothing here needs ImageMagick or xelatex.
"""

from __future__ import annotations

import importlib.util
from pathlib import Path
from typing import Any

import pytest
from srg_sim.loader import CardIndex, LoaderError
from srg_sim.report import classify, finishes, skillreqs, turn
from srg_sim.report.build import build_report, slugify
from srg_sim.report.carddb import ReportCardDB
from srg_sim.report.images import source_webp
from srg_sim.report.model import build_matchup
from srg_sim.report.render import render_report

_STATS = ("power", "agility", "technique", "submission", "grapple", "strike")


def _comp(
    uuid: str, name: str, stats: tuple[int, ...], fins: tuple[str, ...], gimmick: str = ""
) -> dict[str, Any]:
    rec = {
        "card_type": "SingleCompetitorCard",
        "db_uuid": uuid,
        "name": name,
        "division": "World Championship",
        "rules_text": gimmick,
        "tags": [],
        "related_finishes": list(fins),
    }
    rec.update(dict(zip(_STATS, stats, strict=True)))
    return rec


def _finish(
    uuid: str, name: str, atk: str, num: int, text: str = "", tags: tuple[str, ...] = ()
) -> dict[str, Any]:
    return {
        "card_type": "MainDeckCard",
        "db_uuid": uuid,
        "name": name,
        "atk_type": atk,
        "play_order": "Finish",
        "deck_card_number": num,
        "rules_text": text,
        "tags": list(tags),
    }


def _main(uuid: str, name: str, atk: str, num: int, order: str, text: str = "") -> dict[str, Any]:
    return {
        "card_type": "MainDeckCard",
        "db_uuid": uuid,
        "name": name,
        "atk_type": atk,
        "play_order": order,
        "deck_card_number": num,
        "rules_text": text,
        "tags": [],
    }


# Two full 5..10 bijection stat lines (turn odds are exactly 50/50 for any two).
_ALPHA = (5, 10, 9, 6, 7, 8)  # P A T Su G St  (Agility 10)
_BETA = (8, 9, 10, 6, 5, 7)  # (Technique 10)

_RECUR = "Add 1 card from your discard pile to your hand"
_TECH_REQ = "Skill Requirement: Technique 10+"


@pytest.fixture
def db() -> ReportCardDB:
    records = [
        _comp("u-alpha", "Test Alpha", _ALPHA, ("af28", "af29", "af30")),
        _comp("u-beta", "Test Beta", _BETA, ("bf28", "bf29", "bf30")),
        _comp("u-buff", "Buff Bruiser", _ALPHA, ("af28",), gimmick="Your Strike is +5."),
        _comp("u-draw", "Card Sharp", _ALPHA, (), gimmick="Draw 2 cards."),
        _comp("u-recur", "The Recycler", _ALPHA, (), gimmick=_RECUR),
        _comp("u-mug", "The Mugger", _ALPHA, (), gimmick="Your opponent discards 1 card."),
        _comp("u-roll", "Roll Rider", _ALPHA, (), gimmick="Your next turn roll is +1."),
        # signature finishes
        _finish("af28", "Alpha Strike", "Strike", 28, "+2 to Strike"),
        _finish("af29", "Alpha Grip", "Grapple", 29, "+1 to Grapple"),
        _finish("af30", "Alpha Lock", "Submission", 30, ""),
        _finish("bf28", "Beta Strike", "Strike", 28, "+3 to Strike"),
        _finish("bf29", "Beta Grip", "Grapple", 29, "+2 to Grapple"),
        _finish("bf30", "Beta Lock", "Submission", 30, "+1 to Submission"),
        # logoless pool: a Grapple that beats Alpha's signature, a Strike that doesn't
        _finish("lg-g", "Generic Slam", "Grapple", 29, "+6 to Grapple", ("Logoless",)),
        _finish("lg-st", "Generic Jab", "Strike", 28, "+1 to Strike", ("Logoless",)),
        _finish("lg-su", "Generic Hold", "Submission", 30, "+2 to Submission", ("Logoless",)),
        # skill-requirement payoff cards
        _main("sr-a1", "Nimble One", "Strike", 13, "Followup", "Skill Requirement: Agility 8+"),
        _main("sr-a2", "Nimble Two", "Grapple", 14, "Followup", "Skill Requirement: Agility 8+"),
        _main("sr-tech", "Tech Gate", "Submission", 15, "Followup", _TECH_REQ),
        _main("sr-pow", "Power Wall", "Strike", 16, "Followup", "Skill Requirement: Power 10+"),
    ]
    return ReportCardDB(CardIndex(records))


# --- carddb ------------------------------------------------------------------


def test_resolve_competitor_exact_substring_and_ambiguous(db: ReportCardDB) -> None:
    assert db.resolve_competitor("Test Alpha").db_uuid == "u-alpha"
    assert db.resolve_competitor("u-beta").name == "Test Beta"
    assert db.resolve_competitor("Recycler").db_uuid == "u-recur"  # unique substring
    with pytest.raises(LoaderError, match="ambiguous"):
        db.resolve_competitor("Test")  # matches Alpha and Beta
    with pytest.raises(LoaderError, match="no competitor"):
        db.resolve_competitor("Nobody")


def test_finishes_and_logoless_and_bonus(db: ReportCardDB) -> None:
    alpha = db.resolve_competitor("Test Alpha")
    fins = db.finishes_for(alpha)
    assert [f.deck_card_number for f in fins] == [28, 29, 30]  # ordered
    logoless = db.logoless_finishes()
    assert {f.name for f in logoless["Grapple"]} == {"Generic Slam"}
    assert db.finish_bonus(fins[0]) == {"Strike": 2}  # parsed via the real rules parser


# --- turn odds ---------------------------------------------------------------


def test_turn_odds_exact_for_two_bijections_is_fifty_fifty(db: ReportCardDB) -> None:
    a, b = db.resolve_competitor("Test Alpha"), db.resolve_competitor("Test Beta")
    odds = turn.turn_odds(a, b)
    assert odds.method == "exact"
    assert odds.win_a == pytest.approx(0.5) and odds.win_b == pytest.approx(0.5)


def test_exact_turn_odds_math_on_a_lopsided_pair() -> None:
    hi = dict.fromkeys(("Power", "Agility", "Technique", "Submission", "Grapple", "Strike"), 10)
    lo = dict.fromkeys(hi, 5)
    odds = turn._exact_turn_odds(hi, lo)
    assert odds.win_a == 1.0 and odds.win_b == 0.0


def test_turn_odds_routes_to_mc_for_a_roll_gimmick(db: ReportCardDB) -> None:
    buff, beta = db.resolve_competitor("Buff Bruiser"), db.resolve_competitor("Test Beta")
    odds = turn.turn_odds(buff, beta, mc_games=8000, seed=11)
    assert odds.method == "mc" and odds.n == 8000 and odds.ci_a is not None
    assert odds.win_a > 0.5  # a persistent Strike buff lifts the roll (seed-deterministic)
    assert odds.win_a + odds.win_b == pytest.approx(1.0)


# --- finishes ----------------------------------------------------------------


def test_finish_lines_surface_logoless_only_when_strictly_better(db: ReportCardDB) -> None:
    alpha, beta = db.resolve_competitor("Test Alpha"), db.resolve_competitor("Test Beta")
    lines = {ln.atk_type: ln for ln in finishes.finish_lines(db, alpha, beta)}
    assert lines["Grapple"].logoless is not None  # +6 logoless beats +1 signature
    assert lines["Grapple"].logoless.finish.name == "Generic Slam"
    assert lines["Strike"].logoless is None  # +1 logoless worse than +2 signature


def test_finish_curves_are_monotonic_in_crowd_meter(db: ReportCardDB) -> None:
    alpha, beta = db.resolve_competitor("Test Alpha"), db.resolve_competitor("Test Beta")
    for opt in finishes.signature_curves(db, alpha, beta, (1, 2, 3, 4, 5)):
        vals = [opt.odds_at(cm) for cm in (1, 2, 3, 4, 5)]
        assert vals == sorted(vals)  # more crowd meter never lowers finish odds


def test_open_lane_agrees_with_stops(db: ReportCardDB) -> None:
    alpha, beta = db.resolve_competitor("Test Alpha"), db.resolve_competitor("Test Beta")
    for ln in finishes.finish_lines(db, alpha, beta):
        assert ln.open_lane == (not ln.stop["online"])


# --- classify ----------------------------------------------------------------


@pytest.mark.parametrize(
    ("who", "label"),
    [
        ("Card Sharp", "draw"),
        ("The Recycler", "recursion"),
        ("The Mugger", "discard"),
        ("Roll Rider", "turn-advantage"),
    ],
)
def test_classify_heuristic_labels(db: ReportCardDB, who: str, label: str) -> None:
    comp = db.resolve_competitor(who)
    assert classify.classify(comp).label == label


def test_classify_override_wins(db: ReportCardDB) -> None:
    alpha = db.resolve_competitor("Test Alpha")
    ct = classify.classify(alpha, {"u-alpha": {"type": "signature-brawler", "also": ["control"]}})
    assert ct.label == "signature-brawler" and ct.source == "override" and ct.also == ("control",)


# --- skill requirements ------------------------------------------------------


def test_skillreq_parsing() -> None:
    assert skillreqs.parse_requirements("Skill Requirement: Strike 10+, Agility 9+") == (
        ("Strike", 10),
        ("Agility", 9),
    )


def test_priority_cards_rank_by_tier_and_liveness(db: ReportCardDB) -> None:
    # Alpha = Po5 Ag10 Te9 Su6 Gr7 St8; priority eval uses the curated skill_cards.yaml.
    alpha, beta = db.resolve_competitor("Test Alpha"), db.resolve_competitor("Test Beta")
    cards = skillreqs.top_for(alpha, beta)
    names = [c.name for c in cards]
    assert any("Springboard Lion Splash" in n for n in names)  # Strike 8 -> runnable
    spring = next(c for c in cards if "Springboard" in c.name)
    assert spring.live is True  # online: Agility 10 > Strike 8
    assert not any("Poison Stars" in n for n in names)  # needs Strike 9+, Alpha has 8
    ranks = [{"auto": 0, "equal8": 1}[c.tier] for c in cards]
    assert ranks == sorted(ranks)  # auto-includes ranked before Equal-8 stops
    assert "Apocalypse" in skillreqs.personal_choice()  # standing no-requirement note


# --- render + build ----------------------------------------------------------


def test_render_emits_expected_sections(db: ReportCardDB) -> None:
    data = build_matchup(db, "Test Alpha", "Test Beta")
    rst = render_report(data, {})
    tokens = (
        ".. role::",
        "Turn roll:",
        "Finish odds (CM0",
        "Key skill-requirement",
        ".. list-table",
    )
    for token in tokens:
        assert token in rst, token
    assert "Better logoless alternatives" in rst  # Grapple logoless beats signature


def test_source_webp_path_is_sharded_by_uuid_prefix() -> None:
    path = source_webp("aebe9e8baa0046ddbde13690b7c18455", "fullsize", Path("/root"))
    assert path == Path("/root/fullsize/ae/aebe9e8baa0046ddbde13690b7c18455.webp")


def test_slugify() -> None:
    assert slugify("Soborno vs Mrs. Apocalypse") == "soborno-vs-mrs-apocalypse"


@pytest.mark.skipif(importlib.util.find_spec("sphinx") is None, reason="sphinx not installed")
def test_build_report_writes_a_sphinx_html_project(
    db: ReportCardDB, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Drive build_report against the synthetic DB (no real cards.yaml / images).
    monkeypatch.setattr(
        "srg_sim.report.build.ReportCardDB.from_yaml", classmethod(lambda cls, path: db)
    )
    out = build_report("Test Alpha", "Test Beta", out_root=tmp_path, html=True, pdf=False)
    assert (out / "index.rst").exists() and (out / "conf.py").exists()
    assert (out / "_build" / "html" / "index.html").exists()
