//! Getting games into the library.
//! - install: GOG offline installer into <data_dir>/games/<slug>, via
//!   innoextract when available, otherwise silently inside Wine.
//! - add: a game already on disk; its files stay where they are.

use std::fs;
use std::path::{Component, Path, PathBuf};
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
    let task = primary_task(&info).context("no launchable exe in goggame .info")?;

    let slug = slugify(&info.name);
    let dest = games.join(&slug);
    if dest.exists() || Library::load()?.games.contains_key(&slug) {
        fs::remove_dir_all(&staging)?;
        bail!("'{slug}' is already in the library");
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

    let game = Game {
        name: info.name.clone(),
        gog_id: info.game_id.parse().ok(),
        install_dir: dest,
        exe,
        args: task.arguments.split_whitespace().map(String::from).collect(),
        runtime: None,
        saves: Vec::new(),
    };
    register(&slug, game, &rt)
}

/// Adds a game that's already on disk; its files stay where they are.
pub fn add(exe: &Path, name: Option<String>, runtime_id: Option<&str>, args: Vec<String>) -> Result<()> {
    let exe = exe
        .canonicalize()
        .with_context(|| format!("{} not found", exe.display()))?;
    let dir = exe.parent().context("exe has no parent folder")?.to_path_buf();
    let info = read_gog_info(&dir).ok();
    let name = game_name(name, info.as_ref(), &dir).context("can't tell the game's name; pass --name")?;
    let slug = slugify(&name);
    if slug.is_empty() {
        bail!("'{name}' has no usable characters for a slug; pass --name");
    }
    if Library::load()?.games.contains_key(&slug) {
        bail!("'{slug}' is already in the library");
    }

    let rt = runtime::resolve(runtime_id, &exe)?;
    let game = Game {
        name,
        gog_id: info.and_then(|i| i.game_id.parse().ok()),
        install_dir: dir,
        exe: PathBuf::from(exe.file_name().context("bad exe path")?),
        args,
        runtime: None,
        saves: Vec::new(),
    };
    register(&slug, game, &rt)
}

/// --name, else the goggame .info name, else the exe's folder name.
fn game_name(flag: Option<String>, info: Option<&GogInfo>, dir: &Path) -> Option<String> {
    flag.or_else(|| info.map(|i| i.name.clone()))
        .or_else(|| dir.file_name().map(|n| n.to_string_lossy().into_owned()))
}

/// Shared end of install and add: prefix, save paths, restore, library entry.
fn register(slug: &str, mut game: Game, rt: &Runtime) -> Result<()> {
    let prefix = launch::prefix_path(slug)?;
    launch::ensure_prefix(rt, &prefix)?;
    game.runtime = Some(rt.id.clone());
    game.saves = saves::lookup(game.gog_id, &game.name).unwrap_or_else(|e| {
        eprintln!("Warning: save lookup failed: {e:#}");
        Vec::new()
    });

    // Backups survive `megh delete`, so a reinstall can pick them up.
    if let Some(backup) = saves::latest_backup(slug)? {
        let stamp = backup.file_name().unwrap_or_default().to_string_lossy();
        if confirm(&format!("Found a save backup from {stamp}. Restore it?"), true)? {
            saves::restore(&backup, &game, &prefix)?;
            println!("Restored saves from {stamp}.");
        }
    }

    println!("{} is ready as '{slug}'. Run it with: megh run {slug}", game.name);
    let mut library = Library::load()?;
    library.games.insert(slug.to_string(), game);
    library.save()
}

/// Removes a game's prefix and library entry, plus its files if megh installed them.
/// - Saves are backed up first; backups are kept.
pub fn delete(slug: &str) -> Result<()> {
    let mut library = Library::load()?;
    let game = library
        .games
        .get(slug)
        .with_context(|| format!("'{slug}' is not installed"))?;
    let owns_files = owned_by_megh(&game.install_dir, &data_dir()?.join("games"));

    let question = if owns_files {
        format!("Delete {}? Removes its files and Wine prefix; saves are backed up first.", game.name)
    } else {
        format!(
            "Delete {}? Removes its Wine prefix and library entry; game files in {} are kept. Saves are backed up first.",
            game.name,
            game.install_dir.display()
        )
    };
    if !confirm(&question, false)? {
        println!("Cancelled.");
        return Ok(());
    }

    let prefix = launch::prefix_path(slug)?;
    if prefix.exists() {
        saves::backup(slug, game, &prefix)?;
        fs::remove_dir_all(&prefix)?;
    }
    if owns_files && game.install_dir.exists() {
        fs::remove_dir_all(&game.install_dir)?;
    }
    let name = game.name.clone();
    library.games.remove(slug);
    library.save()?;
    println!("Deleted {name}.");
    Ok(())
}

/// Whether megh installed these files (so may delete them).
/// - install_dir comes from a user-editable file, so be strict.
/// - `..` is rejected: starts_with compares components, so "games/../x" would pass.
fn owned_by_megh(install_dir: &Path, games: &Path) -> bool {
    install_dir.starts_with(games)
        && install_dir != games
        && !install_dir.components().any(|c| c == Component::ParentDir)
}

/// The primary FileTask, else the first FileTask.
fn primary_task(info: &GogInfo) -> Option<&PlayTask> {
    let tasks = info.play_tasks.iter().filter(|t| t.kind == "FileTask" && !t.path.is_empty());
    tasks.clone().find(|t| t.is_primary).or_else(|| tasks.clone().next())
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

#[cfg(test)]
mod tests {
    use super::*;

    const SHOGUN_INFO: &str = include_str!("../tests/fixtures/goggame-1104084973.info");

    #[test]
    fn slugs() {
        assert_eq!(slugify("Shogun Showdown"), "shogun-showdown");
        assert_eq!(slugify("Forager"), "forager");
        assert_eq!(slugify("  Baldur's Gate: Enhanced Edition "), "baldur-s-gate-enhanced-edition");
        assert_eq!(slugify("Ys VIII -- Lacrimosa"), "ys-viii-lacrimosa");
    }

    #[test]
    fn reads_real_gog_info() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("goggame-1104084973.info"), SHOGUN_INFO).unwrap();
        let info = read_gog_info(dir.path()).unwrap();
        assert_eq!(info.name, "Shogun Showdown");
        assert_eq!(info.game_id, "1104084973");
        assert_eq!(primary_task(&info).unwrap().path, "ShogunShowdown.exe");
    }

    #[test]
    fn missing_info_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_gog_info(dir.path()).is_err());
    }

    #[test]
    fn primary_task_skips_links_and_prefers_primary() {
        let info: GogInfo = serde_json::from_str(
            r#"{"name": "X", "gameId": "1", "playTasks": [
                {"type": "URLTask", "path": "", "isPrimary": true},
                {"type": "FileTask", "path": "Config.exe"},
                {"type": "FileTask", "path": "bin\\Game.exe", "isPrimary": true}
            ]}"#,
        )
        .unwrap();
        assert_eq!(primary_task(&info).unwrap().path, "bin\\Game.exe");

        let none_primary: GogInfo = serde_json::from_str(
            r#"{"name": "X", "gameId": "1", "playTasks": [
                {"type": "FileTask", "path": "First.exe"},
                {"type": "FileTask", "path": "Second.exe"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(primary_task(&none_primary).unwrap().path, "First.exe");
    }

    #[test]
    fn delete_only_removes_files_megh_installed() {
        let games = Path::new("/data/megh/games");
        assert!(owned_by_megh(&games.join("forager"), games));
        assert!(!owned_by_megh(games, games));
        // Added games: files elsewhere are kept.
        assert!(!owned_by_megh(Path::new("/Users/me/Games/Shogun Showdown"), games));
        assert!(!owned_by_megh(&games.join("../../Documents"), games));
        assert!(!owned_by_megh(&games.join("forager/../.."), games));
    }

    #[test]
    fn game_name_prefers_flag_then_info_then_folder() {
        let info: GogInfo = serde_json::from_str(SHOGUN_INFO).unwrap();
        let dir = Path::new("/Games/shogun-folder");
        assert_eq!(game_name(Some("Mine".into()), Some(&info), dir).unwrap(), "Mine");
        assert_eq!(game_name(None, Some(&info), dir).unwrap(), "Shogun Showdown");
        assert_eq!(game_name(None, None, dir).unwrap(), "shogun-folder");
        assert!(game_name(None, None, Path::new("/")).is_none());
    }
}
