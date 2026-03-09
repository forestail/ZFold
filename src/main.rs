mod cli;
mod crypto;
mod dict;
mod extract;
mod format;
mod list;
mod pack;
mod scan;
mod util;
mod verify;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Commands};

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Pack(args) => pack::run(args),
        Commands::List(args) => list::run(args),
        Commands::Extract(args) => extract::run(args),
        Commands::Verify(args) => verify::run(args),
        Commands::TrainDict(args) => dict::run(args),
    }
}
