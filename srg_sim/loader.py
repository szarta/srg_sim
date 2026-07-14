"""Load the card DB export into an index; resolve a decklist into a Deck (§2).

The **source of authority** is the Postgres DB behind the SRG card-search
website/app; :class:`CardIndex` consumes its read-only YAML export
(``backend/app/cards.yaml``, :data:`DEFAULT_CARDS_YAML`). Card data is *not*
vendored here — every user is assumed to have that repo + DB (DESIGN.md §2).

A **decklist** (``decks/*.yaml``) names a competitor, an entrance, and 30 main
cards; each reference is a bare name, or a ``{name|db_uuid, set?, number?}`` map.
Names are enforced-unique upstream (the mobile app requires disambiguation), so
name resolution normally suffices — but if a name is ever ambiguous the loader
**refuses to guess**, raising :class:`LoaderError` that asks for a ``db_uuid`` or
``set``. Structural problems (unknown ref, non-30 deck, missing skills) raise;
soft issues (a slot ``number`` disagreeing with the card, an ``atk_type`` that
contradicts the number rule) are returned as :class:`LoadedDeck.warnings`.

The loader builds *structural* domain objects only: ``raw_text`` is populated but
``finish_bonuses`` and ``effects`` stay empty — the rules parser (``rules_parser``)
compiles those from ``raw_text`` in a later stage.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Any

import yaml

from srg_sim.cards import (
    AtkType,
    Card,
    Competitor,
    Deck,
    EntranceCard,
    PlayOrder,
    Stats,
)

DEFAULT_CARDS_YAML = (
    Path.home() / "data" / "srg_card_search_website" / "backend" / "app" / "cards.yaml"
)

# Competitor skill columns, in the order Stats() expects them.
_SKILL_KEYS = ("power", "technique", "agility", "submission", "grapple", "strike")

# A decklist reference: a bare name, or a mapping with name/db_uuid/set/number.
Ref = str | dict[str, Any]


class LoaderError(Exception):
    """A decklist could not be resolved against the card index."""


@dataclass
class LoadedDeck:
    """A resolved :class:`~srg_sim.cards.Deck` plus non-fatal load warnings."""

    deck: Deck
    warnings: list[str]


def _rules_text(rec: dict[str, Any]) -> str:
    """The card's rules text, tolerating the ``rules-text`` typo in the export."""
    return rec.get("rules_text") or rec.get("rules-text") or ""


def _norm_ref(ref: Ref) -> dict[str, Any]:
    if isinstance(ref, str):
        return {"name": ref}
    if isinstance(ref, dict):
        return ref
    raise LoaderError(f"reference must be a name or a mapping, got {ref!r}")


class CardIndex:
    """An in-memory index of the card export, keyed by db_uuid and (type, name)."""

    def __init__(self, records: list[dict[str, Any]]) -> None:
        self.records = records
        self._by_uuid: dict[str, dict[str, Any]] = {}
        self._by_name: dict[str, dict[str, list[dict[str, Any]]]] = {}
        for rec in records:
            self._index(rec)

    def _index(self, rec: dict[str, Any]) -> None:
        if "db_uuid" in rec:
            self._by_uuid[rec["db_uuid"]] = rec
        names = self._by_name.setdefault(rec.get("card_type", ""), {})
        names.setdefault(rec.get("name", ""), []).append(rec)

    @classmethod
    def from_yaml(cls, path: str | Path = DEFAULT_CARDS_YAML) -> CardIndex:
        """Build an index from a ``cards.yaml`` export (defaults to the snapshot)."""
        records = yaml.safe_load(Path(path).read_text())
        if not isinstance(records, list):
            raise LoaderError(f"{path}: expected a list of card records")
        return cls(records)

    # -- resolution --------------------------------------------------------

    def _resolve(self, ref: Ref, card_type: str) -> dict[str, Any]:
        norm = _norm_ref(ref)
        if norm.get("db_uuid"):
            return self._resolve_uuid(norm["db_uuid"], card_type)
        name = norm.get("name")
        if not name:
            raise LoaderError(f"reference needs a name or db_uuid: {ref!r}")
        return self._resolve_name(name, card_type, norm.get("set"))

    def _resolve_uuid(self, uuid: str, card_type: str) -> dict[str, Any]:
        rec = self._by_uuid.get(uuid)
        if rec is None:
            raise LoaderError(f"no card with db_uuid {uuid!r}")
        actual = rec.get("card_type")
        if actual != card_type:
            raise LoaderError(f"db_uuid {uuid!r} is a {actual}, expected {card_type}")
        return rec

    def _resolve_name(self, name: str, card_type: str, release_set: str | None) -> dict[str, Any]:
        candidates = self._by_name.get(card_type, {}).get(name, [])
        if release_set is not None:
            candidates = [r for r in candidates if r.get("release_set") == release_set]
        if not candidates:
            where = f" in set {release_set!r}" if release_set else ""
            raise LoaderError(f"no {card_type} named {name!r}{where}")
        if len(candidates) > 1:
            uuids = ", ".join(r.get("db_uuid", "?") for r in candidates)
            raise LoaderError(
                f"ambiguous {card_type} name {name!r}; disambiguate by db_uuid or set: {uuids}"
            )
        return candidates[0]

    # -- domain builders ---------------------------------------------------

    def main_card(self, ref: Ref) -> Card:
        return _build_card(self._resolve(ref, "MainDeckCard"))

    def competitor(self, ref: Ref) -> Competitor:
        return _build_competitor(self._resolve(ref, "SingleCompetitorCard"))

    def entrance(self, ref: Ref) -> EntranceCard:
        return _build_entrance(self._resolve(ref, "EntranceCard"))


def _atk(value: Any) -> AtkType:
    return AtkType(value) if value else AtkType.NONE


def _order(value: Any) -> PlayOrder:
    return PlayOrder(value) if value else PlayOrder.NONE


def _build_card(rec: dict[str, Any]) -> Card:
    number = rec.get("deck_card_number")
    if number is None:
        raise LoaderError(f"main card {rec.get('name')!r} has no deck_card_number")
    return Card(
        db_uuid=rec["db_uuid"],
        name=rec["name"],
        number=number,
        atk_type=_atk(rec.get("atk_type")),
        play_order=_order(rec.get("play_order")),
        finish_bonuses=(),  # filled by rules_parser
        tags=tuple(rec.get("tags") or ()),
        raw_text=_rules_text(rec),
        effects=(),  # filled by rules_parser
    )


def _build_competitor(rec: dict[str, Any]) -> Competitor:
    missing = [k for k in _SKILL_KEYS if rec.get(k) is None]
    if missing:
        raise LoaderError(f"competitor {rec.get('name')!r} is missing skills: {missing}")
    stats = Stats(**{k: int(rec[k]) for k in _SKILL_KEYS})
    return Competitor(
        db_uuid=rec["db_uuid"],
        name=rec["name"],
        division=rec.get("division") or "",
        stats=stats,
        gimmick_text=_rules_text(rec),
        effects=(),
        related_finishes=tuple(rec.get("related_finishes") or ()),
    )


def _build_entrance(rec: dict[str, Any]) -> EntranceCard:
    return EntranceCard(
        db_uuid=rec["db_uuid"],
        name=rec["name"],
        raw_text=_rules_text(rec),
        effects=(),
    )


def load_deck(path: str | Path, index: CardIndex) -> LoadedDeck:
    """Resolve a decklist file into a validated :class:`LoadedDeck` (DESIGN.md §2)."""
    data = yaml.safe_load(Path(path).read_text())
    if not isinstance(data, dict):
        raise LoaderError(f"{path}: decklist must be a mapping")
    for key in ("competitor", "entrance", "cards"):
        if key not in data:
            raise LoaderError(f"{path}: decklist missing '{key}'")
    entries = data["cards"] or []
    cards = tuple(index.main_card(entry) for entry in entries)
    deck = Deck(
        competitor=index.competitor(data["competitor"]),
        entrance=index.entrance(data["entrance"]),
        cards=cards,
    )
    problems = deck.validate()
    if problems:
        raise LoaderError(f"{path}: invalid deck: {'; '.join(problems)}")
    return LoadedDeck(deck=deck, warnings=_deck_warnings(entries, cards))


def _deck_warnings(entries: list[Ref], cards: tuple[Card, ...]) -> list[str]:
    warnings: list[str] = []
    for entry, card in zip(entries, cards, strict=True):
        if isinstance(entry, dict) and entry.get("number") not in (None, card.number):
            warnings.append(
                f"slot number {entry['number']} != {card.name!r} card number {card.number}"
            )
        if not card.atk_type_matches_number():
            warnings.append(
                f"{card.name!r} atk_type {card.atk_type.value} disagrees with number {card.number}"
            )
    return warnings
