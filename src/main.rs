mod install;
mod launch;
mod library;
mod runtime;
mod saves;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use library::Library;

#[derive(Parser)]
#[command(name = "megh", about = "Run Windows games on macOS via Wine + D3DMetal")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Install a GOG offline installer (setup_*.exe)
    Install { setup: PathBuf },
    /// Run an installed game by name, or any Windows .exe by path
    Run {
        game: String,
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
    /// Show save locations and backups for an installed game
    Saves { game: String },
}

/// ~/Library/Application Support/megh
pub fn data_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join("Library/Application Support/megh"))
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Install { setup } => {
            runtime::check_rosetta()?;
            let rt = runtime::ensure()?;
            install::install(&rt, &setup)
        }
        Command::Run { game, hud, log, args } => {
            runtime::check_rosetta()?;
            let rt = runtime::ensure()?;
            let library = Library::load()?;
            if let Some(entry) = library.games.get(&game) {
                let prefix = launch::prefix_path(&game)?;
                saves::backup(&game, entry, &prefix)?;
                let all_args = [entry.args.clone(), args].concat();
                launch::run(&rt, &entry.install_dir.join(&entry.exe), &prefix, hud, log, &all_args)
            } else {
                // A bare exe path: prefix named after the game's folder.
                let exe = PathBuf::from(&game);
                let folder = exe
                    .canonicalize()
                    .with_context(|| format!("'{game}' is not an installed game or an exe path"))?
                    .parent()
                    .and_then(|d| d.file_name())
                    .context("bad exe path")?
                    .to_string_lossy()
                    .into_owned();
                launch::run(&rt, &exe, &launch::prefix_path(&folder)?, hud, log, &args)
            }
        }
        Command::Saves { game } => {
            let library = Library::load()?;
            let entry = library
                .games
                .get(&game)
                .with_context(|| format!("'{game}' is not installed"))?;
            saves::show(&game, entry, &launch::prefix_path(&game)?)
        }
    }
}
