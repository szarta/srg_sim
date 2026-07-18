//! The console consumer — the `srg` binary's command implementations over
//! `srg_core`. Bin-only (gated by the `cli` feature): reads the card DB from YAML,
//! which lib-only / WASM consumers never do.

pub mod commands;
pub mod loader;
pub mod session_cmd;

use std::path::PathBuf;

/// The default card-DB snapshot path (the card-search repo's export; see `CLAUDE.md`),
/// used when `--cards` is not given. Resolves `$HOME` at runtime.
pub fn default_cards_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join("data/srg_card_search_website/backend/app/cards.yaml")
}
