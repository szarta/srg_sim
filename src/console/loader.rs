//! Card-DB loader for the console consumer — a port of the Python `loader.py`.
//!
//! Reads the non-vendored `cards.yaml` export (the card-search repo's snapshot; see
//! `CLAUDE.md`) into an in-memory [`CardIndex`], resolves a decklist against it, and
//! hands back a playable [`Deck`] with compiled IR (via [`enrich_deck`]). This lives
//! in the **bin**, not `srg_core`: YAML (and its C libyaml transitive) is a console/dev
//! concern, kept off lib-only and WASM consumers, which sync the DB as JSON instead.

use anyhow::{anyhow, bail, Context, Result};
use serde_yaml_ng::Value;
use srg_core::cards::{Card, Competitor, Deck, EntranceCard, SKILL_REQUIREMENT_TAG};
use srg_core::ir::{AtkType, PlayOrder, Skill};
use srg_core::parser::{enrich_deck, load_overrides, Overrides};
use srg_core::skills::Skills;
use std::collections::BTreeMap;
use std::path::Path;

/// The pre-expanded Rust override table, embedded from the committed artifact so the
/// console needs no runtime path (mirrors the Python `overrides.yaml` default).
const OVERRIDES_IR: &str = include_str!("../../overrides.ir.json");

/// Load the embedded override table (`db_uuid -> [Effect]`).
pub fn overrides() -> Result<Overrides> {
    load_overrides(OVERRIDES_IR).context("parse embedded overrides.ir.json")
}

/// An in-memory index of the card export, resolvable by `db_uuid` or `(type, name)`.
pub struct CardIndex {
    records: Vec<Value>,
    by_uuid: BTreeMap<String, usize>,
    by_name: BTreeMap<(String, String), Vec<usize>>,
}

impl CardIndex {
    /// Build an index from a `cards.yaml` export.
    pub fn from_yaml(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read card export {}", path.display()))?;
        let value: Value = serde_yaml_ng::from_str(&text)
            .with_context(|| format!("parse card export {}", path.display()))?;
        let records = match value {
            Value::Sequence(rows) => rows,
            _ => bail!("{}: expected a list of card records", path.display()),
        };
        let mut by_uuid = BTreeMap::new();
        let mut by_name: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
        for (i, rec) in records.iter().enumerate() {
            if let Some(uuid) = str_field(rec, "db_uuid") {
                by_uuid.insert(uuid.to_owned(), i);
            }
            let key = (
                str_field(rec, "card_type").unwrap_or("").to_owned(),
                str_field(rec, "name").unwrap_or("").to_owned(),
            );
            by_name.entry(key).or_default().push(i);
        }
        Ok(Self {
            records,
            by_uuid,
            by_name,
        })
    }

    /// The raw records, for the coverage report.
    pub fn records(&self) -> &[Value] {
        &self.records
    }

    /// Whether the DB holds a card with this `db_uuid` — the record validator's
    /// cross-check that an archive names cards this database actually has.
    pub fn has_uuid(&self, uuid: &str) -> bool {
        self.by_uuid.contains_key(uuid)
    }

    /// Resolve a decklist file into a playable, IR-enriched [`Deck`].
    pub fn load_playable(&self, path: &Path, overrides: &Overrides) -> Result<Deck> {
        let deck = self
            .load_deck(path)
            .with_context(|| format!("load deck {}", path.display()))?;
        Ok(enrich_deck(deck, Some(overrides)))
    }

    /// Resolve a decklist file into a [`Deck`] (no IR yet).
    fn load_deck(&self, path: &Path) -> Result<Deck> {
        let text = std::fs::read_to_string(path)?;
        let data: Value = serde_yaml_ng::from_str(&text)?;
        let competitor = self.competitor(field(&data, "competitor")?)?;
        let entrance = self.entrance(field(&data, "entrance")?)?;
        let cards = data
            .get("cards")
            .and_then(Value::as_sequence)
            .map(|s| s.as_slice())
            .unwrap_or(&[]);
        let cards = cards
            .iter()
            .map(|entry| self.main_card(entry))
            .collect::<Result<Vec<_>>>()?;
        Ok(Deck {
            competitor,
            entrance,
            cards,
        })
    }

    /// Rebuild a [`Deck`] from a log header's `db_uuid` list + competitor/entrance names.
    pub fn deck_from_uuids(
        &self,
        competitor: &str,
        entrance: &str,
        card_uuids: &[String],
        overrides: &Overrides,
    ) -> Result<Deck> {
        let cards = card_uuids
            .iter()
            .map(|u| build_card(self.by_uuid(u, "MainDeckCard")?))
            .collect::<Result<Vec<_>>>()?;
        let deck = Deck {
            competitor: build_competitor(self.by_name_one(competitor, "SingleCompetitorCard")?)?,
            entrance: build_entrance(self.by_name_one(entrance, "EntranceCard")?)?,
            cards,
        };
        Ok(enrich_deck(deck, Some(overrides)))
    }

    fn main_card(&self, entry: &Value) -> Result<Card> {
        build_card(self.resolve(entry, "MainDeckCard")?)
    }

    fn competitor(&self, entry: &Value) -> Result<Competitor> {
        build_competitor(self.resolve(entry, "SingleCompetitorCard")?)
    }

    fn entrance(&self, entry: &Value) -> Result<EntranceCard> {
        build_entrance(self.resolve(entry, "EntranceCard")?)
    }

    /// Resolve a decklist reference (a bare name string or a `{name, db_uuid, set}`
    /// mapping) to a raw record of the given card type.
    fn resolve(&self, entry: &Value, card_type: &str) -> Result<&Value> {
        if let Some(uuid) = str_field(entry, "db_uuid") {
            return self.by_uuid(uuid, card_type);
        }
        let name = entry
            .as_str()
            .or_else(|| str_field(entry, "name"))
            .ok_or_else(|| anyhow!("reference needs a name or db_uuid: {entry:?}"))?;
        let set = str_field(entry, "release_set").or_else(|| str_field(entry, "set"));
        self.by_name_filtered(name, card_type, set)
    }

    fn by_uuid(&self, uuid: &str, card_type: &str) -> Result<&Value> {
        let rec = self
            .by_uuid
            .get(uuid)
            .map(|&i| &self.records[i])
            .ok_or_else(|| anyhow!("no card with db_uuid {uuid:?}"))?;
        let actual = str_field(rec, "card_type").unwrap_or("");
        if actual != card_type {
            bail!("db_uuid {uuid:?} is a {actual}, expected {card_type}");
        }
        Ok(rec)
    }

    fn by_name_one(&self, name: &str, card_type: &str) -> Result<&Value> {
        self.by_name_filtered(name, card_type, None)
    }

    fn by_name_filtered(&self, name: &str, card_type: &str, set: Option<&str>) -> Result<&Value> {
        let key = (card_type.to_owned(), name.to_owned());
        let candidates = self.by_name.get(&key).map(Vec::as_slice).unwrap_or(&[]);
        let matches: Vec<&Value> = candidates
            .iter()
            .map(|&i| &self.records[i])
            .filter(|r| set.is_none_or(|s| str_field(r, "release_set") == Some(s)))
            .collect();
        match matches.as_slice() {
            [] => {
                let where_ = set.map(|s| format!(" in set {s:?}")).unwrap_or_default();
                bail!("no {card_type} named {name:?}{where_}")
            }
            [one] => Ok(one),
            many => {
                let uuids: Vec<&str> = many
                    .iter()
                    .map(|r| str_field(r, "db_uuid").unwrap_or("?"))
                    .collect();
                bail!("ambiguous {card_type} name {name:?}; disambiguate by db_uuid or set: {uuids:?}")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Record -> domain builders (port of loader.py's _build_*)
// ---------------------------------------------------------------------------

fn build_card(rec: &Value) -> Result<Card> {
    let name = str_field(rec, "name").unwrap_or("").to_owned();
    let number = rec
        .get("deck_card_number")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("main card {name:?} has no deck_card_number"))?;
    Ok(Card {
        db_uuid: str_field(rec, "db_uuid").unwrap_or("").to_owned(),
        name,
        number,
        atk_type: atk_type(str_field(rec, "atk_type")),
        play_order: play_order(str_field(rec, "play_order")),
        finish_bonuses: BTreeMap::new(),
        tags: card_tags(rec),
        raw_text: rules_text(rec).to_owned(),
        effects: Vec::new(),
    })
}

/// A card's tags, with the DB `spotlight: true` flag folded in as a synthetic
/// `"Spotlight"` tag so gimmicks that reference "a Spotlight" match it through the
/// ordinary `CardFilter { tag }` predicate — no Effect-IR change. Entrances get the
/// same treatment (a later phase).
fn card_tags(rec: &Value) -> Vec<String> {
    let mut tags = string_list(rec, "tags");
    if rec.get("spotlight").and_then(Value::as_bool) == Some(true)
        && !tags.iter().any(|t| t == "Spotlight")
    {
        tags.push("Spotlight".to_owned());
    }
    // A card carrying a `requirements:` block is a "Skill Requirement card"; surface
    // it as a synthetic tag so the stop-resolution `by_skillreq` gate can read it off
    // the stopper without a new `Card` field (mirrors the `spotlight` fold above).
    if rec
        .get("requirements")
        .and_then(Value::as_sequence)
        .is_some_and(|r| !r.is_empty())
        && !tags.iter().any(|t| t == SKILL_REQUIREMENT_TAG)
    {
        tags.push(SKILL_REQUIREMENT_TAG.to_owned());
    }
    tags
}

fn build_competitor(rec: &Value) -> Result<Competitor> {
    let name = str_field(rec, "name").unwrap_or("").to_owned();
    Ok(Competitor {
        db_uuid: str_field(rec, "db_uuid").unwrap_or("").to_owned(),
        name,
        division: str_field(rec, "division").unwrap_or("").to_owned(),
        stats: stats(rec)?,
        gimmick_text: rules_text(rec).to_owned(),
        effects: Vec::new(),
        related_finishes: string_list(rec, "related_finishes"),
    })
}

fn build_entrance(rec: &Value) -> Result<EntranceCard> {
    Ok(EntranceCard {
        db_uuid: str_field(rec, "db_uuid").unwrap_or("").to_owned(),
        name: str_field(rec, "name").unwrap_or("").to_owned(),
        raw_text: rules_text(rec).to_owned(),
        effects: Vec::new(),
    })
}

/// The six competitor skill columns, mapped into a [`Skills`] block.
fn stats(rec: &Value) -> Result<Skills> {
    let name = str_field(rec, "name").unwrap_or("");
    let mut skills = Skills::default();
    for skill in [
        Skill::Power,
        Skill::Agility,
        Skill::Technique,
        Skill::Submission,
        Skill::Grapple,
        Skill::Strike,
    ] {
        let col = skill_column(skill);
        let v = rec
            .get(col)
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow!("competitor {name:?} is missing skill {col:?}"))?;
        set_skill(&mut skills, skill, v);
    }
    Ok(skills)
}

fn skill_column(skill: Skill) -> &'static str {
    match skill {
        Skill::Power => "power",
        Skill::Agility => "agility",
        Skill::Technique => "technique",
        Skill::Submission => "submission",
        Skill::Grapple => "grapple",
        Skill::Strike => "strike",
    }
}

fn set_skill(s: &mut Skills, skill: Skill, v: i64) {
    match skill {
        Skill::Power => s.power = v,
        Skill::Agility => s.agility = v,
        Skill::Technique => s.technique = v,
        Skill::Submission => s.submission = v,
        Skill::Grapple => s.grapple = v,
        Skill::Strike => s.strike = v,
    }
}

// ---------------------------------------------------------------------------
// Value accessors (mirroring the Python dict access + the rules-text typo)
// ---------------------------------------------------------------------------

/// A card's rules text, tolerating the `rules-text` typo in the export.
pub fn rules_text(rec: &Value) -> &str {
    str_field(rec, "rules_text")
        .or_else(|| str_field(rec, "rules-text"))
        .unwrap_or("")
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// A record's `card_type` (e.g. `"MainDeckCard"`), for the coverage filter.
pub fn card_type(rec: &Value) -> Option<&str> {
    str_field(rec, "card_type")
}

/// A record's `db_uuid`, for the coverage override lookup.
pub fn db_uuid(rec: &Value) -> Option<&str> {
    str_field(rec, "db_uuid")
}

fn field<'a>(v: &'a Value, key: &str) -> Result<&'a Value> {
    v.get(key)
        .ok_or_else(|| anyhow!("decklist missing {key:?}"))
}

fn string_list(rec: &Value, key: &str) -> Vec<String> {
    rec.get(key)
        .and_then(Value::as_sequence)
        .map(|s| {
            s.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn atk_type(value: Option<&str>) -> AtkType {
    match value {
        Some("Strike") => AtkType::Strike,
        Some("Grapple") => AtkType::Grapple,
        Some("Submission") => AtkType::Submission,
        _ => AtkType::None,
    }
}

fn play_order(value: Option<&str>) -> PlayOrder {
    match value {
        Some("Lead") => PlayOrder::Lead,
        Some("Followup") => PlayOrder::Followup,
        Some("Finish") => PlayOrder::Finish,
        _ => PlayOrder::None,
    }
}

/// True for a competitor in the top-96 competitive subset (`TOP_DIVISIONS`).
pub fn is_top96(rec: &Value) -> bool {
    matches!(
        str_field(rec, "division"),
        Some("World Championship") | Some("Underworld")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // A minimal in-memory card export: one competitor, one entrance, two main cards
    // (one of them a duplicate name to exercise ambiguity), plus a spectacle the
    // loader must ignore. Enough to check the record -> domain mapping and resolution.
    const CARDS: &str = r#"
- {card_type: SingleCompetitorCard, db_uuid: C-1, name: Test Comp, division: "World Championship",
   power: 5, agility: 4, technique: 3, submission: 2, grapple: 1, strike: 6, rules_text: "gimmick text"}
- {card_type: EntranceCard, db_uuid: E-1, name: Test Entrance, rules_text: "entrance text"}
- {card_type: MainDeckCard, db_uuid: M-1, name: Lead Strike, deck_card_number: 1,
   atk_type: Strike, play_order: Lead, tags: [combo], rules_text: "Your opponent cannot Follow Up."}
- {card_type: MainDeckCard, db_uuid: M-2a, name: Dupe, deck_card_number: 2, atk_type: Grapple, play_order: Followup}
- {card_type: MainDeckCard, db_uuid: M-2b, name: Dupe, deck_card_number: 2, atk_type: Grapple, play_order: Followup}
- {card_type: MainDeckCard, db_uuid: M-3, name: Spot Lead, deck_card_number: 3,
   atk_type: Submission, play_order: Lead, spotlight: true}
- {card_type: MainDeckCard, db_uuid: M-4, name: Req Lead, deck_card_number: 4,
   atk_type: Strike, play_order: Lead, requirements: [{min_strike: 5}]}
- {card_type: SpectacleCard, db_uuid: S-1, name: Ignore Me}
"#;

    fn index() -> CardIndex {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(CARDS.as_bytes()).unwrap();
        CardIndex::from_yaml(f.path()).unwrap()
    }

    #[test]
    fn builds_main_card_from_record() {
        let idx = index();
        let card = idx
            .main_card(&Value::String("Lead Strike".into()))
            .expect("resolve by name");
        assert_eq!(card.db_uuid, "M-1");
        assert_eq!(card.number, 1);
        assert_eq!(card.atk_type, AtkType::Strike);
        assert_eq!(card.play_order, PlayOrder::Lead);
        assert_eq!(card.tags, vec!["combo".to_owned()]);
        assert_eq!(card.raw_text, "Your opponent cannot Follow Up.");
        assert!(card.effects.is_empty(), "loader leaves IR to the parser");
    }

    #[test]
    fn spotlight_flag_folds_into_a_synthetic_tag() {
        let idx = index();
        let card = idx
            .main_card(&Value::String("Spot Lead".into()))
            .expect("resolve by name");
        assert!(
            card.tags.contains(&"Spotlight".to_owned()),
            "got {:?}",
            card.tags
        );
        // A non-spotlight card gets no synthetic tag.
        let plain = idx.main_card(&Value::String("Lead Strike".into())).unwrap();
        assert!(!plain.tags.contains(&"Spotlight".to_owned()));
    }

    #[test]
    fn requirements_block_folds_into_a_skill_requirement_tag() {
        let idx = index();
        let req = idx.main_card(&Value::String("Req Lead".into())).unwrap();
        assert!(
            req.tags.contains(&SKILL_REQUIREMENT_TAG.to_owned()),
            "got {:?}",
            req.tags
        );
        // A card with no requirements gets no synthetic tag.
        let plain = idx.main_card(&Value::String("Lead Strike".into())).unwrap();
        assert!(!plain.tags.contains(&SKILL_REQUIREMENT_TAG.to_owned()));
    }

    #[test]
    fn builds_competitor_and_entrance() {
        let idx = index();
        let comp = idx
            .competitor(&Value::String("Test Comp".into()))
            .expect("competitor");
        assert_eq!(comp.stats.get(Skill::Power), 5);
        assert_eq!(comp.stats.get(Skill::Strike), 6);
        assert_eq!(comp.division, "World Championship");
        assert_eq!(comp.gimmick_text, "gimmick text");
        let ent = idx
            .entrance(&Value::String("Test Entrance".into()))
            .expect("entrance");
        assert_eq!(ent.raw_text, "entrance text");
    }

    #[test]
    fn ambiguous_name_is_an_error() {
        let err = index()
            .main_card(&Value::String("Dupe".into()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("ambiguous"), "got: {err}");
        assert!(err.contains("M-2a") && err.contains("M-2b"), "got: {err}");
    }

    #[test]
    fn uuid_type_mismatch_is_an_error() {
        // C-1 is a competitor; asking for it as a main card must fail.
        let err = index()
            .by_uuid("C-1", "MainDeckCard")
            .unwrap_err()
            .to_string();
        assert!(err.contains("expected MainDeckCard"), "got: {err}");
    }

    #[test]
    fn is_top96_matches_top_divisions() {
        assert!(
            is_top96(&index().records()[0]),
            "World Championship is top-96"
        );
    }
}
