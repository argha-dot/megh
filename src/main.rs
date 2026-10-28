mod launch;
mod runtime;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "megh", about = "Run Windows games on macOS via Wine + D3DMetal")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a Windows .exe
    Run {
        exe: PathBuf,
        /// Show Apple's Metal performance HUD
        #[arg(long)]
        hud: bool,
        /// Write full Wine output to a log file
        #[arg(long)]
        log: bool,
        /// Arguments passed to the game (after --)
        #[arg(last = true)]
        args: Vec<String>,
    },
}

/// ~/Library/Application Support/megh
pub fn data_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join("Library/Application Support/megh"))
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Run { exe, hud, log, args } => {
            runtime::check_rosetta()?;
            let rt = runtime::ensure()?;
            launch::run(&rt, &exe, hud, log, &args)
        }
    }
}
