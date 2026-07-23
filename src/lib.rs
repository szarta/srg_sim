//! `srg-core` — the SRG Supershow match engine: the authoritative, deterministic
//! rules core.
//!
//! This crate is the **substrate** (see [`DESIGN.md`] and
//! [`docs/design/substrate-split.rst`]): it has zero knowledge of its consumers.
//! The console CLI (`src/main.rs`, the `srg` binary) — and, later, the MCP server
//! and the WASM / mobile builds as separate crates — depend on this library, never
//! the reverse. That direction is the substrate boundary, enforced here by the
//! crate graph.
//!
//! The engine executes only the **Effect IR** (`DESIGN.md` §3), emits the
//! **game-log schema** (§8), and is validated against two committed, language-
//! neutral contracts produced by the M-R0 work: the pinned JSON Schemas in
//! `schemas/v1/` and the golden conformance corpus in `fixtures/conformance/`
//! (`docs/design/substrate-split.rst` §6). Determinism rides on a portable
//! `splitmix64` stream so logs are byte-identical with the Python reference oracle.
//!
//! Modules are filled in by the M-R1 port tasks (tracked in `todo-sqlite-cli`):
//! `ir` (§3), `state` (§5), `finish` / `stops` / `engine` (§6), `rng` (§6),
//! `gamelog` (§8), `parser` (§4), `policy` (§7), and `session` (the wire protocol).
//!
//! [`DESIGN.md`]: https://github.com/szarta/srg_sim/blob/main/DESIGN.md
//! [`docs/design/substrate-split.rst`]: https://github.com/szarta/srg_sim/blob/main/docs/design/substrate-split.rst

#![forbid(unsafe_code)]

pub mod cards;
pub mod conditions;
pub mod engine;
pub mod error;
pub mod finish;
pub mod gamelog;
pub mod ir;
pub mod parser;
pub mod policy;
pub mod rng;
pub mod session;
pub mod skills;
pub mod state;
pub mod stops;
#[cfg(feature = "wasm")]
pub mod wasm;

pub use error::{Result, SrgError};
pub use rng::SeededRNG;
pub use skills::Skills;
pub use state::{GameState, PlayerState};
