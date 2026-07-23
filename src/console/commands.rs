//! The console subcommands — a port of `cli.py`'s `play` / `coverage` / `analyze`
//! / `replay`, thin shells over `srg_core`. (`review` and the full matchup-report
//! tooling stay in Python until M-R3; see `docs/design/substrate-split.rst` §7.)

use super::loader::{card_type, db_uuid, is_top96, overrides, rules_text, CardIndex};
use anyhow::{anyhow, bail, Context, Result};
use srg_core::engine::{Engine, GameResult, Yield};
use srg_core::gamelog::{diff, GameLog};
use srg_core::ir::EffectSource;
use srg_core::parser::{coverage, parse_text, CoverageRecord, CoverageReport};
use srg_core::policy::{build_policy, Policies, Policy};
use std::collections::BTreeMap;
use std::path::Path;

const POLICY_NAMES: &str = "random|heuristic|aggressive|smart|newbie";

/// `play A.yaml B.yaml` — one seeded match; print the result, optionally write the log.
pub fn play(
    cards: &Path,
    deck_a: &Path,
    deck_b: &Path,
    seed: u64,
    policies: (&str, &str),
    created: &str,
    out: Option<&Path>,
) -> Result<()> {
    let (policy_a, policy_b) = policies;
    let index = CardIndex::from_yaml(cards)?;
    let ov = overrides()?;
    let da = index.load_playable(deck_a, &ov)?;
    let db = index.load_playable(deck_b, &ov)?;
    let (name_a, name_b) = (da.competitor.name.clone(), db.competitor.name.clone());
    let policies = make_policies(policy_a, policy_b)?;
    let mut engine = Engine::new(
        da,
        db,
        Box::new(policies),
        seed,
        created.to_owned(),
        "sim".into(),
    );
    let result = run(&mut engine)?;
    println!("seed {seed}: {name_a} ({policy_a}) vs {name_b} ({policy_b})");
    println!(
        "result: {} wins by {} in {} turns",
        result.winner, result.reason, result.turns
    );
    if let Some(path) = out {
        engine
            .log
            .write(path)
            .with_context(|| format!("write {}", path.display()))?;
        println!(
            "log: {} ({} events)",
            path.display(),
            engine.log.events.len()
        );
    }
    Ok(())
}

/// `coverage [--top96]` — the rules-parser coverage report over the card DB.
/// The `EffectSource` each card_type's rules text parses as; `None` for the
/// out-of-scope types (Spectacle, CrowdMeter) that carry no parsed match effects.
fn record_source(card_type: &str) -> Option<EffectSource> {
    match card_type {
        "MainDeckCard" => Some(EffectSource::Card),
        "SingleCompetitorCard" | "TornadoCompetitorCard" | "TrioCompetitorCard" => {
            Some(EffectSource::Gimmick)
        }
        "EntranceCard" => Some(EffectSource::Entrance),
        _ => None,
    }
}

/// `cards-ir --out fixtures/parser/cards.ir.json` — emit the parser corpus: every
/// parseable DB record's rules text alongside the Rust-parsed Effect IR. The Rust-
/// native replacement for the retired `scripts/gen_cards_ir.py` (which drove the Python
/// parser oracle). The committed corpus is a frozen regression golden that
/// `tests/parser_parity.rs` holds the parser to — regenerate and review the diff after
/// a deliberate parser change or a card-DB update.
pub fn gen_cards_ir(cards: &Path, out: &Path) -> Result<()> {
    let index = CardIndex::from_yaml(cards)?;
    let ov = overrides()?;
    let mut rows: Vec<serde_json::Value> = Vec::new();
    for rec in index.records() {
        let ct = card_type(rec).unwrap_or("");
        let Some(source) = record_source(ct) else {
            continue;
        };
        let text = rules_text(rec);
        if text.trim().is_empty() {
            continue;
        }
        let uuid = db_uuid(rec).unwrap_or("");
        let effects = parse_text(text, source, Some(uuid), Some(&ov));
        rows.push(serde_json::json!({
            "db_uuid": uuid,
            "card_type": ct,
            "source": source,
            "rules_text": text,
            "effects": effects,
        }));
    }
    rows.sort_by(|a, b| a["db_uuid"].as_str().cmp(&b["db_uuid"].as_str()));
    // A JSON array with one compact, key-sorted record per line — small on disk, yet
    // each record is its own git-diffable line (mirrors the retired Python emitter).
    let lines: Vec<String> = rows
        .iter()
        .map(|r| serde_json::to_string(r).expect("serialize record"))
        .collect();
    std::fs::write(out, format!("[\n{}\n]\n", lines.join(",\n")))
        .with_context(|| format!("write {}", out.display()))?;
    let parsed = rows
        .iter()
        .filter(|r| !r["effects"].as_array().unwrap().is_empty())
        .count();
    println!(
        "{}: {} records ({} with effects) from {}",
        out.display(),
        rows.len(),
        parsed,
        cards.display()
    );
    Ok(())
}

pub fn coverage_report(cards: &Path, top96: bool) -> Result<()> {
    let index = CardIndex::from_yaml(cards)?;
    let ov = overrides()?;
    let main: Vec<CoverageRecord> = index
        .records()
        .iter()
        .filter(|r| card_type(r) == Some("MainDeckCard"))
        .map(|r| CoverageRecord {
            text: rules_text(r),
            db_uuid: db_uuid(r),
        })
        .collect();
    print_coverage("main deck", &coverage(&main, Some(&ov)));
    if top96 {
        let top: Vec<CoverageRecord> = index
            .records()
            .iter()
            .filter(|r| is_top96(r))
            .map(|r| CoverageRecord {
                text: rules_text(r),
                db_uuid: db_uuid(r),
            })
            .collect();
        print_coverage("top-96 competitors", &coverage(&top, Some(&ov)));
    }
    Ok(())
}

/// `parser-fixture` — refresh the curated parser regression sample
/// (`fixtures/parser/clauses.json`) in place. The sample's INPUTS (each case's
/// db_uuid/source/text and the coverage_records) are preserved verbatim; only the
/// parsed OUTPUTS (each case's `expected` IR and the `coverage_golden` counts) are
/// recomputed from the live Rust parser. Post-oracle-retirement this is a Rust
/// regression golden (like `cards.ir.json`), regenerated on legitimate coverage
/// gains — run it after a grammar/override change, then review the diff.
pub fn regen_parser_fixture(path: &Path) -> Result<()> {
    let ov = overrides()?;
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut doc: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;

    // Recompute each case's `expected` from its preserved (source, text, db_uuid).
    let cases = doc["cases"]
        .as_array_mut()
        .ok_or_else(|| anyhow!("clauses fixture missing `cases` array"))?;
    for case in cases.iter_mut() {
        let source = source_of(case["source"].as_str().unwrap_or("card"))?;
        let clause = case["text"].as_str().unwrap_or("").to_owned();
        let uuid = case["db_uuid"].as_str().map(str::to_owned);
        let effects = parse_text(&clause, source, uuid.as_deref(), Some(&ov));
        case["expected"] = serde_json::to_value(&effects)?;
    }

    // Recompute coverage_golden over the preserved coverage_records.
    let records: Vec<(String, String)> = doc["coverage_records"]
        .as_array()
        .ok_or_else(|| anyhow!("clauses fixture missing `coverage_records`"))?
        .iter()
        .map(|r| {
            (
                r["db_uuid"].as_str().unwrap_or("").to_owned(),
                r["rules_text"].as_str().unwrap_or("").to_owned(),
            )
        })
        .collect();
    let recs: Vec<CoverageRecord> = records
        .iter()
        .map(|(u, t)| CoverageRecord {
            text: t,
            db_uuid: if u.is_empty() { None } else { Some(u) },
        })
        .collect();
    let report = coverage(&recs, Some(&ov));
    let top: Vec<serde_json::Value> = report
        .top_unparsed
        .iter()
        .map(|(s, c)| serde_json::json!([s, c]))
        .collect();
    doc["coverage_golden"] = serde_json::json!({
        "total": report.total,
        "grammar": report.grammar,
        "override": report.override_,
        "unsupported": report.unsupported,
        "top_unparsed": top,
    });

    let out = format!("{}\n", serde_json::to_string_pretty(&doc)?);
    std::fs::write(path, out).with_context(|| format!("write {}", path.display()))?;
    println!(
        "{}: refreshed {} cases; coverage total {} grammar {} override {} unsupported {}",
        path.display(),
        doc["cases"].as_array().map_or(0, Vec::len),
        report.total,
        report.grammar,
        report.override_,
        report.unsupported
    );
    Ok(())
}

fn source_of(tag: &str) -> Result<EffectSource> {
    match tag {
        "card" => Ok(EffectSource::Card),
        "gimmick" => Ok(EffectSource::Gimmick),
        "entrance" => Ok(EffectSource::Entrance),
        other => bail!("unknown parser-fixture source {other:?}"),
    }
}

/// `analyze A.yaml B.yaml --games N` — a batch win-rate summary (the full
/// MatchupReport, with finish/turn odds, stays in Python; §7).
pub fn analyze(
    cards: &Path,
    deck_a: &Path,
    deck_b: &Path,
    games: u64,
    seed_start: u64,
    policy_a: &str,
    policy_b: &str,
) -> Result<()> {
    let index = CardIndex::from_yaml(cards)?;
    let ov = overrides()?;
    let da = index.load_playable(deck_a, &ov)?;
    let db = index.load_playable(deck_b, &ov)?;
    let (name_a, name_b) = (da.competitor.name.clone(), db.competitor.name.clone());
    let mut tally = Tally::default();
    for i in 0..games {
        // Fresh policies each game (they may carry per-game state), mirroring the
        // Python factory-per-game batch.
        let policies = make_policies(policy_a, policy_b)?;
        let mut engine = Engine::new(
            da.clone(),
            db.clone(),
            Box::new(policies),
            seed_start + i,
            String::new(),
            "sim".into(),
        );
        tally.record(&run(&mut engine)?);
    }
    println!(
        "analyze: {name_a} ({policy_a}) vs {name_b} ({policy_b}) — {games} games (seeds {seed_start}..{})",
        seed_start + games
    );
    tally.print(games);
    Ok(())
}

/// `replay LOG.jsonl` — re-run a recorded sim log from its header and verify it
/// reproduces byte-for-byte (DESIGN.md §8 determinism).
pub fn replay(cards: &Path, log_path: &Path) -> Result<()> {
    let recorded =
        GameLog::read(log_path).with_context(|| format!("read {}", log_path.display()))?;
    if recorded.header.kind != "sim" {
        bail!(
            "replay verifies sim logs; {} is kind {:?} (a human's decisions aren't re-derivable)",
            log_path.display(),
            recorded.header.kind
        );
    }
    let index = CardIndex::from_yaml(cards)?;
    let ov = overrides()?;
    let a = player(&recorded, "A")?;
    let b = player(&recorded, "B")?;
    let da = index.deck_from_uuids(&a.competitor, &a.entrance, &a.deck, &ov)?;
    let db = index.deck_from_uuids(&b.competitor, &b.entrance, &b.deck, &ov)?;
    let policies = make_policies(&a.policy, &b.policy)?;
    let mut engine = Engine::new(
        da,
        db,
        Box::new(policies),
        recorded.header.seed,
        recorded.header.created.clone(),
        recorded.header.kind.clone(),
    );
    run(&mut engine)?;
    let diffs = diff(&recorded, &engine.log);
    if diffs.is_empty() {
        println!(
            "replay OK: {} reproduces ({} events)",
            log_path.display(),
            engine.log.events.len()
        );
        return Ok(());
    }
    for d in diffs.iter().take(20) {
        println!("  {d}");
    }
    bail!("replay MISMATCH: {} differing record(s)", diffs.len());
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// `audit A.yaml B.yaml` — the deck-testing harness. Reports each deck's static
/// coverage gaps (unmodeled clauses), then plays `games` seeded matches scanning
/// each for crashes, runtime `Unsupported` no-ops, and non-decisive endings — the
/// go-to check when a new deck is added. With `capture`, banks the first decisive
/// game as a conformance replay-golden.
#[allow(clippy::too_many_arguments)]
pub fn audit(
    cards: &Path,
    deck_a: &Path,
    deck_b: &Path,
    games: u64,
    seed_start: u64,
    policy_a: &str,
    policy_b: &str,
    capture: Option<&Path>,
) -> Result<()> {
    let index = CardIndex::from_yaml(cards)?;
    let ov = overrides()?;
    let da = index.load_playable(deck_a, &ov)?;
    let db = index.load_playable(deck_b, &ov)?;

    // 1. Static coverage: the clauses each deck's own cards leave Unsupported.
    println!("=== deck coverage (unmodeled clauses) ===");
    for (path, deck) in [(deck_a, &da), (deck_b, &db)] {
        let gaps = deck_unsupported(deck);
        println!(
            "{} [{}]: {} unmodeled clause(s)",
            deck.competitor.name,
            path.display(),
            gaps.len()
        );
        for g in &gaps {
            println!("    · {g}");
        }
    }

    // 2. Playtest: seeded games, catching per-seed panics and scanning logs.
    println!(
        "\n=== playtest: {} vs {} — {games} games (seeds {seed_start}..{}), {policy_a} vs {policy_b} ===",
        da.competitor.name,
        db.competitor.name,
        seed_start + games
    );
    let mut decisive = 0u64;
    let mut turn_cap = 0u64;
    let mut crashed: Vec<u64> = Vec::new();
    let mut runtime: BTreeMap<String, u64> = BTreeMap::new();
    let mut captured = false;
    for i in 0..games {
        let seed = seed_start + i;
        match play_one(&da, &db, policy_a, policy_b, seed) {
            Err(_) => crashed.push(seed),
            Ok((result, log)) => {
                if result.reason == "turn_cap" {
                    turn_cap += 1;
                } else {
                    decisive += 1;
                }
                for ev in &log.events {
                    if let srg_core::gamelog::Event::Unsupported { raw, .. } = ev {
                        *runtime.entry(raw.clone()).or_default() += 1;
                    }
                }
                if let Some(path) = capture {
                    if !captured && result.reason != "turn_cap" {
                        write_fixture(path, &da, &db, policy_a, policy_b, seed, &log)?;
                        println!(
                            "captured conformance fixture (seed {seed}) -> {}",
                            path.display()
                        );
                        captured = true;
                    }
                }
            }
        }
    }
    println!(
        "  decisive: {decisive}   turn-cap: {turn_cap}   crashed: {}",
        crashed.len()
    );
    if !crashed.is_empty() {
        println!("  CRASHED seeds: {crashed:?}");
    }
    if runtime.is_empty() {
        println!("  runtime Unsupported no-ops: none");
    } else {
        println!("  runtime Unsupported no-ops (clause × times fired):");
        let mut rows: Vec<_> = runtime.iter().collect();
        rows.sort_by(|a, b| b.1.cmp(a.1));
        for (raw, n) in rows {
            println!("    {n:5} × {raw}");
        }
    }
    Ok(())
}

/// One seeded local game, isolated so a panic reports the offending seed instead of
/// aborting the whole audit. Returns the result and its log.
fn play_one(
    da: &srg_core::cards::Deck,
    db: &srg_core::cards::Deck,
    policy_a: &str,
    policy_b: &str,
    seed: u64,
) -> Result<(GameResult, GameLog)> {
    let (da, db) = (da.clone(), db.clone());
    let (pa, pb) = (policy_a.to_owned(), policy_b.to_owned());
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<_> {
        let policies = make_policies(&pa, &pb)?;
        let mut engine = Engine::new(
            da,
            db,
            Box::new(policies),
            seed,
            String::new(),
            "sim".into(),
        );
        let result = run(&mut engine)?;
        Ok((result, engine.log))
    }));
    match outcome {
        Ok(res) => res,
        Err(_) => bail!("panic on seed {seed}"),
    }
}

/// The raw text of every `Unsupported` node in a deck's compiled IR (gimmick,
/// entrance, and each card) — the deck's modeling gaps.
fn deck_unsupported(deck: &srg_core::cards::Deck) -> Vec<String> {
    let mut out = Vec::new();
    let mut scan = |effects: &[srg_core::ir::Effect]| {
        let v = serde_json::to_value(effects).unwrap_or(serde_json::Value::Null);
        collect_unsupported(&v, &mut out);
    };
    scan(&deck.competitor.effects);
    scan(&deck.entrance.effects);
    for c in &deck.cards {
        scan(&c.effects);
    }
    out
}

fn collect_unsupported(v: &serde_json::Value, out: &mut Vec<String>) {
    match v {
        serde_json::Value::Object(m) => {
            if m.get("@type").and_then(|t| t.as_str()) == Some("Unsupported") {
                if let Some(raw) = m.get("raw_text").and_then(|r| r.as_str()) {
                    out.push(raw.to_owned());
                }
            }
            for val in m.values() {
                collect_unsupported(val, out);
            }
        }
        serde_json::Value::Array(a) => a.iter().for_each(|x| collect_unsupported(x, out)),
        _ => {}
    }
}

/// Write a conformance replay-golden from a completed sim game: decks + the
/// per-player decisions (extracted from the log's `decision` events) + the log.
fn write_fixture(
    path: &Path,
    da: &srg_core::cards::Deck,
    db: &srg_core::cards::Deck,
    policy_a: &str,
    policy_b: &str,
    seed: u64,
    log: &GameLog,
) -> Result<()> {
    let mut decisions: BTreeMap<String, Vec<serde_json::Value>> = BTreeMap::new();
    for ev in &log.events {
        if let srg_core::gamelog::Event::Decision { player, chosen, .. } = ev {
            decisions
                .entry(player.clone())
                .or_default()
                .push(chosen.clone());
        }
    }
    let label = format!(
        "{}_{}_{}_s{seed}",
        policy_a,
        slug(&da.competitor.name),
        slug(&db.competitor.name)
    );
    let fixture = serde_json::json!({
        "fixture_schema": 1,
        "label": label,
        "seed": seed,
        "kind": "sim",
        "policies": {"A": policy_a, "B": policy_b},
        "decks": {"A": da, "B": db},
        "decisions": {"A": decisions.get("A").cloned().unwrap_or_default(),
                      "B": decisions.get("B").cloned().unwrap_or_default()},
        "log": log.canonical(),
    });
    let text = serde_json::to_string_pretty(&fixture)? + "\n";
    std::fs::write(path, text).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// A filename-safe lowercase slug (letters/digits → keep, else `_`).
fn slug(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// Run a fully-local match to completion; a suspension means a policy declined to
/// choose (a bug for local policies — remote seats belong to the Session/MCP path).
fn run(engine: &mut Engine) -> Result<GameResult> {
    engine.play().map_err(|Yield(req)| {
        anyhow!(
            "engine suspended awaiting a {:?} decision — local policies must always choose",
            req.point
        )
    })
}

fn make_policies(a: &str, b: &str) -> Result<Policies> {
    Ok(Policies::new(named_policy(a)?, named_policy(b)?))
}

fn named_policy(name: &str) -> Result<Box<dyn Policy>> {
    build_policy(name).ok_or_else(|| anyhow!("unknown policy {name:?}; choose from {POLICY_NAMES}"))
}

fn player<'a>(log: &'a GameLog, key: &str) -> Result<&'a srg_core::gamelog::PlayerInfo> {
    log.header
        .players
        .get(key)
        .ok_or_else(|| anyhow!("log header has no player {key:?}"))
}

fn print_coverage(label: &str, report: &CoverageReport) {
    println!(
        "\n{label}: {} clauses ({:.1}% parsed)",
        report.total,
        report.rate() * 100.0
    );
    println!("  grammar      {:6}", report.grammar);
    println!("  override     {:6}", report.override_);
    println!("  unsupported  {:6}", report.unsupported);
    if !report.top_unparsed.is_empty() {
        println!("  top unparsed shapes:");
        for (shape, count) in report.top_unparsed.iter().take(15) {
            println!("    {count:5}  {shape}");
        }
    }
}

/// Running win/reason tallies for `analyze`.
#[derive(Default)]
struct Tally {
    a: u64,
    b: u64,
    draws: u64,
    turns: i64,
    reasons: BTreeMap<String, u64>,
}

impl Tally {
    fn record(&mut self, r: &GameResult) {
        match r.winner.as_str() {
            "A" => self.a += 1,
            "B" => self.b += 1,
            _ => self.draws += 1,
        }
        self.turns += r.turns;
        *self.reasons.entry(r.reason.clone()).or_default() += 1;
    }

    fn print(&self, games: u64) {
        if games == 0 {
            println!("  (no games)");
            return;
        }
        let pct = |n: u64| 100.0 * n as f64 / games as f64;
        println!("  A wins   {:5}  ({:.1}%)", self.a, pct(self.a));
        println!("  B wins   {:5}  ({:.1}%)", self.b, pct(self.b));
        println!("  draws    {:5}  ({:.1}%)", self.draws, pct(self.draws));
        println!("  avg turns {:.1}", self.turns as f64 / games as f64);
        println!("  by reason:");
        for (reason, count) in &self.reasons {
            println!("    {count:5}  {reason}");
        }
    }
}
