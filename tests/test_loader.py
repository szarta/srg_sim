"""Tests for the card loader (DESIGN.md §2): resolution, builders, decklists.

The core resolution/builder logic runs against a small synthetic in-memory index
(no card DB needed). A second group loads the shipped ``decks/*.yaml`` against the
real export and is skipped when that snapshot is absent (e.g. CI).
"""

from __future__ import annotations

import functools
from pathlib import Path
from typing import Any

import pytest
from srg_sim.cards import AtkType, PlayOrder
from srg_sim.loader import (
    DEFAULT_CARDS_YAML,
    CardIndex,
    LoaderError,
    load_deck,
)

# --- synthetic index (offline) ---------------------------------------------

_ATK = {0: "Submission", 1: "Strike", 2: "Grapple"}


def _main(number: int, name: str, **extra: Any) -> dict[str, Any]:
    rec = {
        "card_type": "MainDeckCard",
        "db_uuid": f"m{number:02d}",
        "name": name,
        "deck_card_number": number,
        "atk_type": _ATK[number % 3],
        "play_order": "Finish" if number >= 28 else ("Lead" if number <= 12 else "Followup"),
        "rules_text": f"text {number}",
        "tags": [],
    }
    rec.update(extra)
    return rec


def _synth_records() -> list[dict[str, Any]]:
    records: list[dict[str, Any]] = [_main(n, f"M{n:02d}") for n in range(1, 31)]
    records.append(
        {
            "card_type": "SingleCompetitorCard",
            "db_uuid": "comp1",
            "name": "Test Comp",
            "division": "Worlds",
            "power": 10,
            "technique": 6,
            "agility": 5,
            "submission": 8,
            "grapple": 9,
            "strike": 7,
            "rules_text": "a gimmick",
            "related_finishes": ["m28", "m29", "m30"],
        }
    )
    records.append(
        {  # a competitor missing a skill (power) -> should fail to build
            "card_type": "SingleCompetitorCard",
            "db_uuid": "comp2",
            "name": "Broken Comp",
            "technique": 6,
            "agility": 5,
            "submission": 8,
            "grapple": 9,
            "strike": 7,
        }
    )
    records.append(
        {
            "card_type": "EntranceCard",
            "db_uuid": "ent1",
            "name": "Test Ent",
            "rules_text": "walk in",
        }
    )
    # An ambiguous name: two main cards share a name across sets.
    records.append(_main(5, "Twin", db_uuid="twinA", release_set="Alpha"))
    records.append(_main(5, "Twin", db_uuid="twinB", release_set="Beta"))
    return records


@pytest.fixture
def index() -> CardIndex:
    return CardIndex(_synth_records())


def test_resolve_by_name(index: CardIndex) -> None:
    card = index.main_card("M01")
    assert card.db_uuid == "m01"
    assert card.number == 1
    assert card.atk_type is AtkType.STRIKE
    assert card.play_order is PlayOrder.LEAD


def test_resolve_by_uuid(index: CardIndex) -> None:
    assert index.main_card({"db_uuid": "m28"}).name == "M28"


def test_wrong_type_for_uuid_raises(index: CardIndex) -> None:
    with pytest.raises(LoaderError, match="is a SingleCompetitorCard, expected MainDeckCard"):
        index.main_card({"db_uuid": "comp1"})


def test_unknown_name_raises(index: CardIndex) -> None:
    with pytest.raises(LoaderError, match="no MainDeckCard named 'Nope'"):
        index.main_card("Nope")


def test_unknown_uuid_raises(index: CardIndex) -> None:
    with pytest.raises(LoaderError, match="no card with db_uuid"):
        index.main_card({"db_uuid": "zzz"})


def test_ambiguous_name_refuses_to_guess(index: CardIndex) -> None:
    # The mobile app enforces name disambiguation; the loader mirrors that.
    with pytest.raises(LoaderError, match="ambiguous MainDeckCard name 'Twin'"):
        index.main_card("Twin")


def test_set_disambiguates_a_shared_name(index: CardIndex) -> None:
    assert index.main_card({"name": "Twin", "set": "Beta"}).db_uuid == "twinB"


def test_ref_without_name_or_uuid_raises(index: CardIndex) -> None:
    with pytest.raises(LoaderError, match="needs a name or db_uuid"):
        index.main_card({"set": "Alpha"})


def test_build_competitor_fields(index: CardIndex) -> None:
    comp = index.competitor("Test Comp")
    assert comp.stats.to_dict()["Power"] == 10
    assert comp.gimmick_text == "a gimmick"
    assert comp.related_finishes == ("m28", "m29", "m30")


def test_missing_skill_competitor_raises(index: CardIndex) -> None:
    with pytest.raises(LoaderError, match="missing skills.*power"):
        index.competitor("Broken Comp")


def test_build_entrance(index: CardIndex) -> None:
    ent = index.entrance("Test Ent")
    assert ent.name == "Test Ent"
    assert ent.raw_text == "walk in"


def test_loader_leaves_effects_for_the_parser(index: CardIndex) -> None:
    # Structural load only: raw_text is set, effects/finish_bonuses stay empty.
    card = index.main_card("M28")
    assert card.raw_text == "text 28"
    assert card.effects == ()
    assert card.finish_bonuses == ()


def test_rules_text_typo_fallback() -> None:
    idx = CardIndex(
        [{"card_type": "EntranceCard", "db_uuid": "e", "name": "E", "rules-text": "hyphenated"}]
    )
    assert idx.entrance("E").raw_text == "hyphenated"


# --- decklist loading (offline, synthetic) ---------------------------------


def _write_decklist(path: Path, lines: list[str]) -> Path:
    path.write_text("\n".join(lines) + "\n")
    return path


def test_load_deck_offline(tmp_path: Path, index: CardIndex) -> None:
    lines = ["competitor: Test Comp", "entrance: Test Ent", "cards:"]
    lines += [f"  - {{number: {n}, name: M{n:02d}}}" for n in range(1, 31)]
    result = load_deck(_write_decklist(tmp_path / "d.yaml", lines), index)
    assert result.deck.is_valid()
    assert result.warnings == []
    assert result.deck.competitor.name == "Test Comp"


def test_load_deck_missing_key(tmp_path: Path, index: CardIndex) -> None:
    path = _write_decklist(tmp_path / "d.yaml", ["competitor: Test Comp", "entrance: Test Ent"])
    with pytest.raises(LoaderError, match="missing 'cards'"):
        load_deck(path, index)


def test_load_deck_incomplete_is_invalid(tmp_path: Path, index: CardIndex) -> None:
    lines = ["competitor: Test Comp", "entrance: Test Ent", "cards:"]
    lines += [f"  - {{number: {n}, name: M{n:02d}}}" for n in range(1, 5)]  # only 4 cards
    with pytest.raises(LoaderError, match="invalid deck"):
        load_deck(_write_decklist(tmp_path / "d.yaml", lines), index)


def test_slot_number_mismatch_warns(tmp_path: Path, index: CardIndex) -> None:
    lines = ["competitor: Test Comp", "entrance: Test Ent", "cards:"]
    # Deliberately mislabel M01 (real number 1) as slot number 2.
    lines.append("  - {number: 2, name: M01}")
    lines += [f"  - {{number: {n}, name: M{n:02d}}}" for n in range(2, 31) if n != 1]
    result = load_deck(_write_decklist(tmp_path / "d.yaml", lines), index)
    assert any("slot number 2" in w for w in result.warnings)


def test_atk_type_number_mismatch_warns() -> None:
    # A card whose atk_type contradicts its number rule surfaces a warning.
    from srg_sim.loader import _deck_warnings

    idx = CardIndex([_main(1, "Bad", atk_type="Grapple")])  # number 1 -> should be Strike
    card = idx.main_card("Bad")
    warnings = _deck_warnings([{"number": 1, "name": "Bad"}], (card,))
    assert any("disagrees with number" in w for w in warnings)


# --- real card DB integration (skipped when the export is absent) ----------


@functools.lru_cache(maxsize=1)
def _real_index() -> CardIndex:
    return CardIndex.from_yaml(DEFAULT_CARDS_YAML)


requires_db = pytest.mark.skipif(
    not DEFAULT_CARDS_YAML.exists(), reason=f"card export not available: {DEFAULT_CARDS_YAML}"
)
_DECKS = Path(__file__).resolve().parent.parent / "decks"


@requires_db
def test_real_index_builds() -> None:
    assert len(_real_index().records) > 1000


@requires_db
@pytest.mark.parametrize("name", ["bull", "fae"])
def test_shipped_decklists_load_clean(name: str) -> None:
    result = load_deck(_DECKS / f"{name}.yaml", _real_index())
    assert result.deck.is_valid()
    assert result.warnings == []


@requires_db
def test_real_decks_play_a_deterministic_game() -> None:
    from srg_sim.engine import Engine
    from srg_sim.policy import HeuristicPolicy

    def run() -> Any:
        idx = _real_index()
        bull = load_deck(_DECKS / "bull.yaml", idx).deck
        fae = load_deck(_DECKS / "fae.yaml", idx).deck
        eng = Engine(bull, fae, HeuristicPolicy(), HeuristicPolicy(), seed=7, created="x")
        eng.play()
        return eng

    first, second = run(), run()
    assert first.result == second.result
    assert first.state.log.to_lines() == second.state.log.to_lines()
