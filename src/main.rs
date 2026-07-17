//! `srg` — the console CLI, the first consumer of `srg-core` (DESIGN.md §9).
//!
//! A thin shell over the library. The real subcommands (`play`, `coverage`,
//! `analyze`, `replay`, `review`) land with the M-R1 consumer task (#76); this
//! scaffold wires up `clap` and proves the lib/bin boundary compiles.

use clap::{Parser, Subcommand};

/// SRG Supershow match engine — command-line interface.
#[derive(Parser)]
#[command(name = "srg", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print engine build info (placeholder until the M-R1 consumers land).
    Info,
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Info => {
            println!(
                "srg-core {} — engine scaffold (M-R1 in progress)",
                env!("CARGO_PKG_VERSION")
            );
        }
    }
    Ok(())
}
