//! `srg` — the console CLI, the first consumer of `srg-core` (DESIGN.md §9).
//!
//! A thin shell over the library (`src/console`): resolve decklists against the card
//! DB, then `play` a seeded match, report parser `coverage`, `analyze` a batch of
//! games, or `replay` a recorded sim log and verify it reproduces. The full
//! matchup-report tooling and post-game `review` stay in Python until M-R3
//! (`docs/design/substrate-split.md` §7). The lib/bin boundary — the substrate never
//! importing a consumer — is enforced by the crate graph.

mod console;

use clap::{Parser, Subcommand};
use console::{commands, default_cards_path, session_cmd};
use std::path::PathBuf;

/// SRG Supershow match engine — command-line interface.
#[derive(Parser)]
#[command(name = "srg", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Play one seeded match between two decklists.
    Play {
        /// Decklist YAML for side A.
        deck_a: PathBuf,
        /// Decklist YAML for side B.
        deck_b: PathBuf,
        #[arg(long, default_value_t = 0)]
        seed: u64,
        #[arg(long, default_value = "heuristic")]
        policy_a: String,
        #[arg(long, default_value = "heuristic")]
        policy_b: String,
        /// Header timestamp (kept out of the engine).
        #[arg(long, default_value = "")]
        created: String,
        /// Write the JSONL game log here.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Path to the cards.yaml export (defaults to the DB snapshot).
        #[arg(long)]
        cards: Option<PathBuf>,
    },
    /// Rules-parser coverage report over the card DB (DESIGN.md §4).
    Coverage {
        /// Also report the top-96 competitor subset.
        #[arg(long)]
        top96: bool,
        #[arg(long)]
        cards: Option<PathBuf>,
    },
    /// Batch N seeded games and print a win-rate summary.
    Analyze {
        deck_a: PathBuf,
        deck_b: PathBuf,
        #[arg(long, default_value_t = 100)]
        games: u64,
        #[arg(long, default_value_t = 0)]
        seed_start: u64,
        #[arg(long, default_value = "heuristic")]
        policy_a: String,
        #[arg(long, default_value = "heuristic")]
        policy_b: String,
        #[arg(long)]
        cards: Option<PathBuf>,
    },
    /// Re-run a recorded sim log and verify it reproduces byte-for-byte.
    Replay {
        /// Recorded JSONL game log.
        log: PathBuf,
        #[arg(long)]
        cards: Option<PathBuf>,
    },
    /// Drive a resumable match over the decision protocol (the MCP substrate).
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
    /// Emit the parser corpus (rules text + Rust-parsed IR) for parser-parity regression.
    CardsIr {
        /// Where to write the corpus (the committed frozen golden).
        #[arg(long, default_value = "fixtures/parser/cards.ir.json")]
        out: PathBuf,
        /// Path to the cards.yaml export (defaults to the DB snapshot).
        #[arg(long)]
        cards: Option<PathBuf>,
    },
    /// Refresh the curated parser regression sample in place (recompute each
    /// case's `expected` IR + `coverage_golden` from the live parser).
    ParserFixture {
        /// The sample to refresh.
        #[arg(long, default_value = "fixtures/parser/clauses.json")]
        path: PathBuf,
    },
    /// Print engine build info.
    Info,
}

/// Stateless, snapshot-threaded steps of a [`Session`] (see `src/console/session_cmd.rs`).
#[derive(Subcommand)]
enum SessionAction {
    /// Open a match; print the first `{snapshot, step}` JSON.
    Open {
        deck_a: PathBuf,
        deck_b: PathBuf,
        #[arg(long, default_value_t = 0)]
        seed: u64,
        /// `remote` (a human/agent decides via submit) or a policy name (local AI).
        #[arg(long, default_value = "remote")]
        seat_a: String,
        #[arg(long, default_value = "heuristic")]
        seat_b: String,
        #[arg(long, default_value = "")]
        created: String,
        #[arg(long)]
        cards: Option<PathBuf>,
    },
    /// Answer the outstanding decision with `legal[K]` (snapshot on stdin).
    Submit {
        #[arg(long)]
        choice_index: usize,
    },
    /// Re-print the current `{snapshot, step}` without advancing (snapshot on stdin).
    Observe,
}

fn cards_or_default(cards: Option<PathBuf>) -> PathBuf {
    cards.unwrap_or_else(default_cards_path)
}

fn run_session(action: SessionAction) -> anyhow::Result<()> {
    match action {
        SessionAction::Open {
            deck_a,
            deck_b,
            seed,
            seat_a,
            seat_b,
            created,
            cards,
        } => session_cmd::open(
            &cards_or_default(cards),
            (&deck_a, &deck_b),
            seed,
            (&seat_a, &seat_b),
            &created,
        ),
        SessionAction::Submit { choice_index } => session_cmd::submit(choice_index),
        SessionAction::Observe => session_cmd::observe(),
    }
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Play {
            deck_a,
            deck_b,
            seed,
            policy_a,
            policy_b,
            created,
            out,
            cards,
        } => commands::play(
            &cards_or_default(cards),
            &deck_a,
            &deck_b,
            seed,
            (&policy_a, &policy_b),
            &created,
            out.as_deref(),
        ),
        Command::Coverage { top96, cards } => {
            commands::coverage_report(&cards_or_default(cards), top96)
        }
        Command::Analyze {
            deck_a,
            deck_b,
            games,
            seed_start,
            policy_a,
            policy_b,
            cards,
        } => commands::analyze(
            &cards_or_default(cards),
            &deck_a,
            &deck_b,
            games,
            seed_start,
            &policy_a,
            &policy_b,
        ),
        Command::Replay { log, cards } => commands::replay(&cards_or_default(cards), &log),
        Command::CardsIr { out, cards } => commands::gen_cards_ir(&cards_or_default(cards), &out),
        Command::ParserFixture { path } => commands::regen_parser_fixture(&path),
        Command::Session { action } => run_session(action),
        Command::Info => {
            println!(
                "srg-core {} — console CLI over srg-core (M-R1)",
                env!("CARGO_PKG_VERSION")
            );
            Ok(())
        }
    }
}
