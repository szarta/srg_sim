//! `srg record` / `srg validate-record` — produce and check portable match records
//! (`srg_core::record`, pinned in `schemas/v1/match_record.schema.json`).
//!
//! `record` plays a seeded match through a [`Session`] and writes the `full` record:
//! the observable-frame sequence a viewer replays, plus the compact seed that
//! re-simulates it. `validate-record` checks any record — including an `observer`
//! archive a user hand-authored from a real-life match — for structural problems,
//! and with `--cards` also resolves every card uuid against the card DB.

use super::loader::{overrides, CardIndex};
use anyhow::{bail, Context, Result};
use srg_core::record::{CardRef, MatchRecord, RecordMeta, Validation};
use srg_core::session::{Seat, Session};
use std::collections::BTreeMap;
use std::path::Path;

/// `record A.yaml B.yaml --out rec.json` — play one seeded match, write its record.
pub fn record(
    cards: &Path,
    decks: (&Path, &Path),
    seed: u64,
    policies: (&str, &str),
    meta: RecordMeta,
    out: &Path,
) -> Result<()> {
    let index = CardIndex::from_yaml(cards)?;
    let ov = overrides()?;
    let (deck_a, deck_b) = decks;
    let (da, db) = (
        index.load_playable(deck_a, &ov)?,
        index.load_playable(deck_b, &ov)?,
    );
    let seats: BTreeMap<String, Seat> = [("A", policies.0), ("B", policies.1)]
        .iter()
        .map(|(key, spec)| ((*key).to_owned(), Seat::from_spec(spec)))
        .collect();
    let created = meta.created.clone();
    let (session, _) = Session::open(da, db, seats, seed, created, "sim".into())
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let record = session
        .record(meta)
        .context("session did not finish (a seat asked for a decision)")?;
    record
        .write(out)
        .with_context(|| format!("write {}", out.display()))?;
    let result = &record.result;
    println!(
        "{}: {} frames, {} wins by {} in {} turns",
        out.display(),
        record.frames.len(),
        result.winner,
        result.reason,
        result.turns
    );
    report(&record.validate());
    Ok(())
}

/// `validate-record rec.json` — structural check; `--cards` also resolves uuids.
pub fn validate(path: &Path, cards: Option<&Path>) -> Result<()> {
    let record = MatchRecord::read(path).map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut result = record.validate();
    if let Some(cards) = cards {
        check_uuids(&record, &CardIndex::from_yaml(cards)?, &mut result);
    }
    println!(
        "{}: {:?} record, {} frames, {}",
        path.display(),
        record.kind,
        record.frames.len(),
        if record.is_replayable() {
            "re-simulatable"
        } else {
            "playback only"
        }
    );
    report(&result);
    if !result.is_valid() {
        bail!("{}: {} error(s)", path.display(), result.errors.len());
    }
    Ok(())
}

fn report(result: &Validation) {
    for warning in &result.warnings {
        println!("  warning: {warning}");
    }
    for error in &result.errors {
        println!("  ERROR: {error}");
    }
    if result.errors.is_empty() {
        println!("  ok ({} warning(s))", result.warnings.len());
    }
}

/// Cross-check every card reference against the card DB: an unknown uuid means the
/// archive names a card this database does not have.
fn check_uuids(record: &MatchRecord, index: &CardIndex, result: &mut Validation) {
    let mut unknown: Vec<String> = Vec::new();
    for card in refs(record) {
        if !card.card.is_empty() && !index.has_uuid(&card.card) && !unknown.contains(&card.card) {
            unknown.push(card.card.clone());
        }
    }
    for uuid in unknown {
        result
            .errors
            .push(format!("card uuid {uuid} is not in the card DB"));
    }
}

/// Every card reference in a record: participants (competitor/entrance/decklist)
/// and every frame's boards, discards, and actions.
fn refs(record: &MatchRecord) -> Vec<&CardRef> {
    let mut out: Vec<&CardRef> = Vec::new();
    for p in record.players.values() {
        out.push(&p.competitor);
        out.extend(p.entrance.iter());
        out.extend(p.deck.iter());
    }
    for frame in &record.frames {
        for player in frame.players.values() {
            out.extend(player.in_play.iter().chain(&player.discard));
        }
        out.extend(action_refs(&frame.action));
    }
    out
}

fn action_refs(action: &srg_core::record::Action) -> Vec<&CardRef> {
    use srg_core::record::Action;
    match action {
        Action::Play { card, .. } => vec![card],
        Action::Stop { card, stopped, .. } => vec![card, stopped],
        Action::FinishAttempt { finish, .. } => vec![finish],
        Action::Discard { cards, .. }
        | Action::Bury { cards, .. }
        | Action::Search { cards, .. } => cards.iter().collect(),
        _ => Vec::new(),
    }
}
