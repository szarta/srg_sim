//! `srg session open|submit|observe` — the stateless, snapshot-threaded face of the
//! resumable [`Session`] (`docs/design/substrate-split.md` §3.3/§4), the substrate the
//! MCP server (`mcp_server/`) drives.
//!
//! A [`Session`] is a pure function of its serializable [`SessionSnapshot`], so each
//! subcommand is one stateless step: `open` builds a fresh session from two decks and
//! prints `{snapshot, step}`; `submit` and `observe` read a snapshot from **stdin**,
//! restore it, act, and print the next `{snapshot, step}`. The caller (the MCP server,
//! a script) owns the snapshot between calls — no server-side session state here.

use super::loader::{overrides, CardIndex};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::json;
use srg_core::engine::{DecisionResponse, Step};
use srg_core::session::{Seat, Session, SessionSnapshot};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

/// `session open` — start a match over two decklists and a seat per player, printing
/// the first `{snapshot, step}` (parked at a decision, or `done` if both seats are AI).
pub fn open(
    cards: &Path,
    decks: (&Path, &Path),
    seed: u64,
    seats: (&str, &str),
    created: &str,
) -> Result<()> {
    let index = CardIndex::from_yaml(cards)?;
    let ov = overrides()?;
    let da = index.load_playable(decks.0, &ov)?;
    let db = index.load_playable(decks.1, &ov)?;
    let (seat_a, seat_b) = (Seat::from_spec(seats.0), Seat::from_spec(seats.1));
    // A remote seat means a human/agent takes at least one decision — the §8 "real" mark.
    let kind = if is_remote(&seat_a) || is_remote(&seat_b) {
        "real"
    } else {
        "sim"
    };
    let map = BTreeMap::from([("A".to_owned(), seat_a), ("B".to_owned(), seat_b)]);
    let (session, step) = Session::open(da, db, map, seed, created.to_owned(), kind.to_owned())
        .map_err(|e| anyhow!("open session: {e}"))?;
    emit(&session, &step)
}

/// `session submit --choice-index K` — restore the snapshot on stdin, answer the
/// outstanding decision with its `legal[K]`, and print the next `{snapshot, step}`.
pub fn submit(choice_index: usize) -> Result<()> {
    let (mut session, step) = Session::restore(read_snapshot()?).map_err(|e| anyhow!("{e}"))?;
    let Step::Decision(req) = step else {
        bail!("session is not awaiting a decision (already done)");
    };
    let chosen = req.legal.get(choice_index).cloned().ok_or_else(|| {
        anyhow!(
            "choice-index {choice_index} out of range 0..{}",
            req.legal.len()
        )
    })?;
    let next = session.submit(DecisionResponse {
        request_id: req.request_id,
        chosen,
    });
    emit(&session, &next)
}

/// `session observe` — restore the snapshot on stdin and print its current
/// `{snapshot, step}` without advancing (idempotent re-fetch).
pub fn observe() -> Result<()> {
    let (session, step) = Session::restore(read_snapshot()?).map_err(|e| anyhow!("{e}"))?;
    emit(&session, &step)
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn is_remote(seat: &Seat) -> bool {
    matches!(seat, Seat::Remote { .. })
}

/// Read a [`SessionSnapshot`] as JSON from stdin.
fn read_snapshot() -> Result<SessionSnapshot> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("read snapshot from stdin")?;
    serde_json::from_str(&buf).context("parse session snapshot")
}

/// Print `{snapshot, step}` as one JSON line (the caller threads `snapshot` back in).
fn emit(session: &Session, step: &Step) -> Result<()> {
    let out = json!({
        "snapshot": session.snapshot(),
        "step": step.to_json(),
    });
    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use srg_core::engine::{DecisionRequest, GameResult};

    #[test]
    fn seat_maps_remote_and_local() {
        assert!(matches!(Seat::from_spec("remote"), Seat::Remote { .. }));
        match Seat::from_spec("smart") {
            Seat::Local { policy } => assert_eq!(policy, "smart"),
            _ => panic!("a policy name is a local seat"),
        }
    }

    // The MCP server (`mcp_server/server.py`) reads exactly these shapes.
    #[test]
    fn step_json_done_shape() {
        let step = Step::Done(GameResult {
            winner: "A".into(),
            reason: "finish".into(),
            turns: 12,
        });
        let j = step.to_json();
        assert_eq!(j["kind"], "done");
        assert_eq!(j["result"]["winner"], "A");
        assert_eq!(j["result"]["reason"], "finish");
        assert_eq!(j["result"]["turns"], 12);
    }

    #[test]
    fn step_json_decision_shape() {
        let step = Step::Decision(DecisionRequest {
            request_id: "r1".into(),
            seq: 3,
            viewer: "A".into(),
            point: "turn_action".into(),
            legal: vec![json!({"kind": "pass"})],
            observable_state: json!({"turn": 1}),
        });
        let j = step.to_json();
        assert_eq!(j["kind"], "decision");
        assert_eq!(j["request"]["request_id"], "r1");
        assert_eq!(j["request"]["viewer"], "A");
        assert_eq!(j["request"]["point"], "turn_action");
        assert_eq!(j["request"]["legal"][0]["kind"], "pass");
        assert_eq!(j["request"]["observable_state"]["turn"], 1);
    }
}
