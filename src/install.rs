//! Installs a GOG offline installer into <data_dir>/games/<slug>.
//! - Extracts with innoextract when available.
//! - Otherwise runs the installer silently inside Wine.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::library::{Game, Library};
use crate::runtime::{self, Runtime};
use crate::{confirm, data_dir, launch, saves};

/// goggame-<id>.info, shipped with every GOG game.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GogInfo {
    name: String,
    game_id: String,
    #[serde(default)]
    play_tasks: Vec<PlayTask>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlayTask {
    #[serde(default)]
    is_primary: bool,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    arguments: String,
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum Method {
    /// innoextract if available, else Wine
    Auto,
    /// innoextract only
    Extract,
    /// Run the installer silently in Wine
    Wine,
}

/// `runtime_id` overrides the auto-picked runtime (installer and game).
pub fn install(setup: &Path, method: Method, runtime_id: Option<&str>) -> Result<()> {
    let setup = setup
        .canonicalize()
        .with_context(|| format!("{} not found", setup.display()))?;
    let extract = match method {
        Method::Auto => innoextract_available()?,
        Method::Extract if innoextract_available()? => true,
        Method::Extract => bail!("innoextract is not installed"),
        Method::Wine => false,
    };

    let games = data_dir()?.join("games");
    // Per-process names so parallel installs don't clobber each other.
    let staging_name = format!(".installing-{}", std::process::id());
    let staging = games.join(&staging_name);
    let staging_prefix = launch::prefix_path(&staging_name)?;
    // Leftovers from an interrupted install that had the same pid.
    for dir in [&staging, &staging_prefix] {
        if dir.exists() {
            fs::remove_dir_all(dir)?;
        }
    }
    fs::create_dir_all(&staging)?;

    let installer_rt = if extract {
        let status = Command::new("innoextract")
            .args(["--gog", "--exclude-temp", "--silent", "--progress=1", "-d"])
            .arg(&staging)
            .arg(&setup)
            .status()?;
        if !status.success() {
            bail!("innoextract failed");
        }
        None
    } else {
        let rt = runtime::resolve(runtime_id, &setup)?;
        wine_install(&rt, &setup, &staging, &staging_prefix)?;
        Some(rt)
    };

    let info = read_gog_info(&staging)?;
    let task = info
        .play_tasks
        .iter()
        .filter(|t| t.kind == "FileTask" && !t.path.is_empty())
        .max_by_key(|t| t.is_primary)
        .context("no launchable exe in goggame .info")?;

    let slug = slugify(&info.name);
    let dest = games.join(&slug);
    if dest.exists() {
        fs::remove_dir_all(&staging)?;
        bail!("{slug} is already installed at {}", dest.display());
    }
    fs::rename(&staging, &dest)?;

    let exe = PathBuf::from(task.path.replace('\\', "/"));
    let rt = runtime::resolve(runtime_id, &dest.join(&exe))?;
    let prefix = launch::prefix_path(&slug)?;
    if staging_prefix.exists() {
        // Reuse the installer's prefix only if the game runs on the same Wine.
        let same_rt = installer_rt.as_ref().is_some_and(|r| r.id == rt.id);
        if same_rt && !prefix.exists() {
            fs::rename(&staging_prefix, &prefix)?;
        } else {
            fs::remove_dir_all(&staging_prefix)?;
        }
    }
    launch::ensure_prefix(&rt, &prefix)?;

    let gog_id = info.game_id.parse().ok();
    let saves = saves::lookup(gog_id, &info.name).unwrap_or_else(|e| {
        eprintln!("Warning: save lookup failed: {e:#}");
        Vec::new()
    });

    let game = Game {
        name: info.name.clone(),
        gog_id,
        install_dir: dest,
        exe,
        args: task.arguments.split_whitespace().map(String::from).collect(),
        runtime: Some(rt.id.clone()),
        saves,
    };
    // Backups survive `megh delete`, so a reinstall can pick them up.
    if let Some(backup) = saves::latest_backup(&slug)? {
        let stamp = backup.file_name().unwrap_or_default().to_string_lossy();
        if confirm(&format!("Found a save backup from {stamp}. Restore it?"), true)? {
            saves::restore(&backup, &game, &prefix)?;
            println!("Restored saves from {stamp}.");
        }
    }

    let mut library = Library::load()?;
    library.games.insert(slug.clone(), game);
    library.save()?;

    println!("Installed {} as '{slug}'. Run it with: megh run {slug}", info.name);
    Ok(())
}

/// Removes a game's files, prefix and library entry.
/// - Saves are backed up first; backups are kept.
pub fn delete(slug: &str) -> Result<()> {
    let mut library = Library::load()?;
    let game = library
        .games
        .get(slug)
        .with_context(|| format!("'{slug}' is not installed"))?;
    // install_dir comes from a user-editable file; never delete outside megh.
    let games = data_dir()?.join("games");
    if !game.install_dir.starts_with(&games) {
        bail!("refusing to delete {}: not inside {}", game.install_dir.display(), games.display());
    }

    let question = format!("Delete {}? Removes its files and Wine prefix; saves are backed up first.", game.name);
    if !confirm(&question, false)? {
        println!("Cancelled.");
        return Ok(());
    }

    let prefix = launch::prefix_path(slug)?;
    if prefix.exists() {
        saves::backup(slug, game, &prefix)?;
        fs::remove_dir_all(&prefix)?;
    }
    if game.install_dir.exists() {
        fs::remove_dir_all(&game.install_dir)?;
    }
    let name = game.name.clone();
    library.games.remove(slug);
    library.save()?;
    println!("Deleted {name}.");
    Ok(())
}

/// Offers `brew install innoextract` if it's missing but brew exists.
fn innoextract_available() -> Result<bool> {
    let found = |cmd: &str| Command::new(cmd).arg("--version").output().is_ok();
    if found("innoextract") {
        return Ok(true);
    }
    if !found("brew") {
        return Ok(false);
    }
    if !confirm("innoextract not found. Install it with Homebrew?", true)? {
        return Ok(false);
    }
    let status = Command::new("brew").args(["install", "innoextract"]).status()?;
    Ok(status.success() && found("innoextract"))
}

fn wine_install(rt: &Runtime, setup: &Path, dir: &Path, prefix: &Path) -> Result<()> {
    println!("Running installer in Wine (silent) ...");
    launch::ensure_prefix(rt, prefix)?;
    // Wine maps Z: to /.
    let win_dir = format!("Z:{}", dir.display()).replace('/', "\\");
    let status = launch::wine(rt, prefix)
        .arg(setup)
        .args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/NOICONS"])
        .arg(format!("/DIR={win_dir}"))
        .env("WINEDEBUG", "-all")
        .status()?;
    if !status.success() {
        bail!("installer failed with {status}");
    }
    // The prefix gets renamed next; wait for wineserver to let go of it.
    launch::wine_server_wait(rt, prefix)
}

fn read_gog_info(dir: &Path) -> Result<GogInfo> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if name.starts_with("goggame-") && name.ends_with(".info") {
            return Ok(serde_json::from_str(&fs::read_to_string(&path)?)?);
        }
    }
    bail!("no goggame-*.info found; only GOG installers are supported")
}

fn slugify(name: &str) -> String {
    name.to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}
