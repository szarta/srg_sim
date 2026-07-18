//! WASM/JS bindings — the browser face of the resumable [`Session`] (M-R2,
//! `docs/design/substrate-split.md` §7). Enabled by the `wasm` feature; the web
//! presentation layer (`web/`) is *purely presentational* over what this exposes.
//!
//! The same crate that runs natively server-side (authoritative PvP) compiles to
//! `wasm32` and drives a match in the browser. The seam is unchanged: only
//! [`observable`](crate::state::GameState::observable) state — carried on each
//! [`DecisionRequest`] — crosses into JS; the seed and hidden zones never do.
//!
//! Everything crosses as **JSON strings** (`JSON.parse`/`stringify` on the JS side),
//! so the only added dependency is `wasm-bindgen` itself — no `serde-wasm-bindgen`,
//! no `js-sys`. Decks arrive already IR-enriched (the web client syncs the card DB as
//! JSON from get-diced.com); this layer never parses YAML.

use crate::cards::Deck;
use crate::engine::{DecisionResponse, Step};
use crate::session::{Seat, Session, SessionSnapshot};
use serde_json::json;
use std::collections::BTreeMap;
use wasm_bindgen::prelude::*;

/// A live match in the browser: a [`Session`] plus its current [`Step`], driven by
/// `submit`. Construct with [`WasmSession::open`] or [`WasmSession::restore`].
#[wasm_bindgen]
pub struct WasmSession {
    inner: Session,
    step: Step,
}

#[wasm_bindgen]
impl WasmSession {
    /// Open a match. `deck_a`/`deck_b` are IR-enriched [`Deck`] JSON; `seats` is a
    /// JSON object `{"A": "remote"|<policy>, "B": ...}` (`"remote"` = a browser/agent
    /// seat that answers via [`submit`](Self::submit)); `seed` is the RNG seed.
    ///
    /// Returns a session parked at the first decision (or already `done` if no seat
    /// is remote). Errors (bad deck JSON, invalid deck, unknown policy) surface as a
    /// thrown JS `Error`.
    pub fn open(
        deck_a: &str,
        deck_b: &str,
        seats: &str,
        seed: u64,
    ) -> Result<WasmSession, JsError> {
        let deck_a: Deck = parse(deck_a, "deck_a")?;
        let deck_b: Deck = parse(deck_b, "deck_b")?;
        let seat_map: BTreeMap<String, String> = parse(seats, "seats")?;
        let seats = seat_map
            .iter()
            .map(|(k, v)| (k.clone(), Seat::from_spec(v)))
            .collect();
        let kind = match_kind(&seat_map);
        let (inner, step) = Session::open(deck_a, deck_b, seats, seed, String::new(), kind)
            .map_err(|e| JsError::new(&e.to_string()))?;
        Ok(WasmSession { inner, step })
    }

    /// Rebuild a session from a [`snapshot`](Self::snapshot) JSON string, resuming at
    /// the same step. The whole match is a pure function of this snapshot.
    pub fn restore(snapshot: &str) -> Result<WasmSession, JsError> {
        let snap: SessionSnapshot = parse(snapshot, "snapshot")?;
        let (inner, step) = Session::restore(snap).map_err(|e| JsError::new(&e.to_string()))?;
        Ok(WasmSession { inner, step })
    }

    /// Answer the outstanding decision with option `choice_index` of its `legal`
    /// list, advancing the match. Returns the next [`step`](Self::step) JSON. Errors
    /// if the session is already done or the index is out of range.
    pub fn submit(&mut self, choice_index: usize) -> Result<String, JsError> {
        let Step::Decision(req) = &self.step else {
            return Err(JsError::new("session is already done"));
        };
        let chosen = req.legal.get(choice_index).cloned().ok_or_else(|| {
            JsError::new(&format!(
                "choice_index {choice_index} out of range 0..{}",
                req.legal.len()
            ))
        })?;
        let response = DecisionResponse {
            request_id: req.request_id.clone(),
            chosen,
        };
        self.step = self.inner.submit(response);
        Ok(self.step.to_json().to_string())
    }

    /// The current step as JSON: `{"kind":"decision","request":{viewer, point, legal,
    /// observable_state, ...}}` or `{"kind":"done","result":{winner, reason, turns}}`.
    pub fn step(&self) -> String {
        self.step.to_json().to_string()
    }

    /// A self-contained snapshot JSON string (seed, decks, seats, decisions) — feed it
    /// to [`restore`](Self::restore) to reconstruct this exact match.
    pub fn snapshot(&self) -> String {
        json!(self.inner.snapshot()).to_string()
    }
}

fn parse<T: serde::de::DeserializeOwned>(json: &str, what: &str) -> Result<T, JsError> {
    serde_json::from_str(json).map_err(|e| JsError::new(&format!("parse {what}: {e}")))
}

/// A remote seat means a human/agent takes at least one decision — the §8 `"real"` mark.
fn match_kind(seats: &BTreeMap<String, String>) -> String {
    if seats.values().any(|v| v == "remote") {
        "real".to_owned()
    } else {
        "sim".to_owned()
    }
}
