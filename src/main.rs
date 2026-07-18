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
use console::{commands, default_cards_path};
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
    /// Print engine build info.
    Info,
}

fn cards_or_default(cards: Option<PathBuf>) -> PathBuf {
    cards.unwrap_or_else(default_cards_path)
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
        Command::Info => {
            println!(
                "srg-core {} — console CLI over srg-core (M-R1)",
                env!("CARGO_PKG_VERSION")
            );
            Ok(())
        }
    }
}
