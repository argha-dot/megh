mod install;
mod launch;
mod library;
mod runtime;
mod saves;

use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use library::Library;

#[derive(Parser)]
#[command(name = "megh", about = "Install and run Windows games on Apple Silicon Macs with Wine")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Install a GOG offline installer (setup_*.exe)
    Install {
        setup: PathBuf,
        #[arg(long, value_enum, default_value_t = install::Method::Auto)]
        method: install::Method,
        /// Runtime id to use instead of auto-picking (see: megh runtimes)
        #[arg(long = "runtime", value_name = "ID")]
        runtime_id: Option<String>,
    },
    /// Add a game that's already on disk (its files stay where they are)
    Add {
        exe: PathBuf,
        /// Game name (default: from goggame-*.info, else the exe's folder name)
        #[arg(long)]
        name: Option<String>,
        /// Runtime id to use instead of auto-picking (see: megh runtimes)
        #[arg(long = "runtime", value_name = "ID")]
        runtime_id: Option<String>,
        /// Arguments always passed to the game (after --)
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// List installed games
    List,
    /// Delete an installed game (saves are backed up first)
    Delete { game: String },
    /// Run an installed game by name, or any Windows .exe by path
    Run {
        game: String,
        /// Show Apple's Metal performance HUD
        #[arg(long)]
        hud: bool,
        /// Write full Wine output to a log file
        #[arg(long)]
        log: bool,
        /// Runtime id for this launch only (see: megh runtimes)
        #[arg(long = "runtime", value_name = "ID")]
        runtime_id: Option<String>,
        /// Arguments passed to the game (after --)
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Show save locations and backups for an installed game
    Saves {
        game: String,
        /// Restore the latest backup (overwrites current saves)
        #[arg(long)]
        restore: bool,
    },
    /// List known Wine runtimes and their status
    Runtimes,
}

/// ~/Library/Application Support/megh
pub fn data_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join("Library/Application Support/megh"))
}

/// Asks a yes/no question; empty input picks `default`.
pub fn confirm(question: &str, default: bool) -> Result<bool> {
    print!("{question} {} ", if default { "[Y/n]" } else { "[y/N]" });
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(parse_answer(&line, default))
}

fn parse_answer(input: &str, default: bool) -> bool {
    match input.trim().to_lowercase().as_str() {
        "y" | "yes" => true,
        "n" | "no" => false,
        _ => default,
    }
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Install { setup, method, runtime_id } => {
            runtime::check_rosetta()?;
            install::install(&setup, method, runtime_id.as_deref())
        }
        Command::Add { exe, name, runtime_id, args } => {
            runtime::check_rosetta()?;
            install::add(&exe, name, runtime_id.as_deref(), args)
        }
        Command::List => {
            let library = Library::load()?;
            if library.games.is_empty() {
                println!("No games installed. Add one with: megh install <setup.exe>");
            }
            let width = library.games.keys().map(String::len).max().unwrap_or(0);
            for (slug, game) in &library.games {
                println!("{slug:width$}  {}", game.name);
            }
            Ok(())
        }
        Command::Delete { game } => install::delete(&game),
        Command::Run { game, hud, log, runtime_id, args } => {
            runtime::check_rosetta()?;
            let library = Library::load()?;
            if let Some(entry) = library.games.get(&game) {
                let exe = entry.install_dir.join(&entry.exe);
                let id = runtime_id.as_deref().or(entry.runtime.as_deref());
                let rt = runtime::resolve(id, &exe)?;
                let prefix = launch::prefix_path(&game)?;
                saves::backup(&game, entry, &prefix)?;
                let all_args = [entry.args.clone(), args].concat();
                launch::run(&rt, &exe, &prefix, hud, log, &all_args)
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
                let rt = runtime::resolve(runtime_id.as_deref(), &exe)?;
                launch::run(&rt, &exe, &launch::prefix_path(&folder)?, hud, log, &args)
            }
        }
        Command::Runtimes => runtime::list(),
        Command::Saves { game, restore } => {
            let library = Library::load()?;
            let entry = library
                .games
                .get(&game)
                .with_context(|| format!("'{game}' is not installed"))?;
            let prefix = launch::prefix_path(&game)?;
            if !restore {
                return saves::show(&game, entry, &prefix);
            }
            let backup = saves::latest_backup(&game)?.context("no backups to restore")?;
            let stamp = backup.file_name().unwrap_or_default().to_string_lossy();
            if confirm(&format!("Restore saves from {stamp}? This overwrites current saves."), false)? {
                saves::restore(&backup, entry, &prefix)?;
                println!("Restored saves from {stamp}.");
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_answer;

    #[test]
    fn answers() {
        for yes in ["y", "Y", "yes", " YES \n"] {
            assert!(parse_answer(yes, false), "{yes:?}");
        }
        for no in ["n", "No", "no\n"] {
            assert!(!parse_answer(no, true), "{no:?}");
        }
    }

    #[test]
    fn empty_or_unknown_uses_default() {
        for input in ["", "\n", "maybe"] {
            assert!(parse_answer(input, true));
            assert!(!parse_answer(input, false));
        }
    }
}
