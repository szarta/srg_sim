//! Crate-wide error type (the `thiserror` shape shared across our Rust tools).

use thiserror::Error;

/// Convenience alias for fallible `srg-core` operations.
pub type Result<T> = std::result::Result<T, SrgError>;

/// Everything that can go wrong inside the engine. Rules the parser cannot map do
/// **not** live here — they surface as an `Unsupported` node in the Effect IR and
/// in the coverage report (DESIGN.md §3/§4), never as a silently dropped rule.
#[derive(Debug, Error)]
pub enum SrgError {
    /// A decklist failed an integrity or resolution check (DESIGN.md §2).
    #[error("deck error: {0}")]
    Deck(String),

    /// A recorded log or fixture diverged from what the engine reproduced
    /// (DESIGN.md §8 replay; the conformance harness, substrate-split.md §6).
    #[error("conformance mismatch: {0}")]
    Conformance(String),

    /// Reading or writing a file failed.
    #[error("I/O error for '{path}': {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// A JSON (de)serialization failure — card IR, game log, or a fixture.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}
