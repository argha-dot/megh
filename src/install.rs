//! Installs a GOG offline installer into <data_dir>/games/<slug>.
//! - Extracts with innoextract when available.
//! - Otherwise runs the installer silently inside Wine.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::library::{Game, Library};
use crate::runtime::Runtime;
use crate::{data_dir, launch, saves};

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

pub fn install(rt: &Runtime, setup: &Path) -> Result<()> {
    let setup = setup
        .canonicalize()
        .with_context(|| format!("{} not found", setup.display()))?;
    let games = data_dir()?.join("games");
    let staging = games.join(".installing");
    let staging_prefix = launch::prefix_path(".installing")?;
    // Leftovers from an interrupted install.
    for dir in [&staging, &staging_prefix] {
        if dir.exists() {
            fs::remove_dir_all(dir)?;
        }
    }
    fs::create_dir_all(&staging)?;

    if innoextract_available()? {
        let status = Command::new("innoextract")
            .args(["--gog", "--exclude-temp", "--silent", "--progress=1", "-d"])
            .arg(&staging)
            .arg(&setup)
            .status()?;
        if !status.success() {
            bail!("innoextract failed");
        }
    } else {
        wine_install(rt, &setup, &staging, &staging_prefix)?;
    }

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

    let prefix = launch::prefix_path(&slug)?;
    if staging_prefix.exists() && !prefix.exists() {
        fs::rename(&staging_prefix, &prefix)?;
    }
    launch::ensure_prefix(rt, &prefix)?;

    let gog_id = info.game_id.parse().ok();
    let saves = saves::lookup(gog_id, &info.name).unwrap_or_else(|e| {
        eprintln!("Warning: save lookup failed: {e:#}");
        Vec::new()
    });

    let mut library = Library::load()?;
    library.games.insert(
        slug.clone(),
        Game {
            name: info.name.clone(),
            gog_id,
            install_dir: dest,
            exe: PathBuf::from(task.path.replace('\\', "/")),
            args: task.arguments.split_whitespace().map(String::from).collect(),
            saves,
        },
    );
    library.save()?;

    println!("Installed {} as '{slug}'. Run it with: megh run {slug}", info.name);
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
    print!("innoextract not found. Install it with Homebrew? [Y/n] ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    if line.trim().eq_ignore_ascii_case("n") {
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
